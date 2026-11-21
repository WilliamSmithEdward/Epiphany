using System;
using System.Collections.Generic;
using System.Linq;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Epiphany.Excel
{
    /// <summary>One server, as the worksheet functions see it. Cell reads and writes asked for
    /// within a moment of each other travel as one request each, and dimensions are fetched
    /// once. Nothing here throws: every answer is text, a number, <see cref="KeyError"/> or
    /// <see cref="Unavailable"/>.</summary>
    public sealed class Connection : IDisposable
    {
        /// <summary>What PAfE shows for a reference that cannot be resolved.</summary>
        public const string KeyError = "*KEY_ERR";

        /// <summary>The answer when the server cannot be reached, refuses the session, or has
        /// the cube locked. The worksheet shows it as #N/A.</summary>
        public static readonly object Unavailable = new object();

        // How long to wait for more cells before sending those already asked for.
        private static readonly TimeSpan Gather = TimeSpan.FromMilliseconds(20);

        private sealed class Cell
        {
            public string Cube;
            public string[] At;
            public object Value;
            public readonly TaskCompletionSource<object> Answer =
                new TaskCompletionSource<object>(TaskCreationOptions.RunContinuationsAsynchronously);
        }

        private sealed class Reply
        {
            public int Status;
            public object Body;
            public bool Ok => Status >= 200 && Status < 300;
        }

        private readonly HttpClient http;
        private readonly object gate = new object();
        private readonly Timer timer;
        // One batch is sent at a time, so that writes reach the server in the order made.
        private readonly SemaphoreSlim sending = new SemaphoreSlim(1);
        private List<Cell> reads = new List<Cell>();
        private List<Cell> writes = new List<Cell>();
        private Dictionary<string, Task<Reply>> dimensions = new Dictionary<string, Task<Reply>>();

        public string Address { get; }

        /// <summary>Raised when the server refuses a request for want of a session.</summary>
        public event Action SignInNeeded;

        /// <summary>Raised when an answer comes from another version of the model than the
        /// one before it did. The dimensions held have been forgotten by then.</summary>
        public event Action ModelChanged;

        // The model version the last answer came from, as the server's header gave it.
        private string version;

        /// <summary>Raised when the server no longer knows the transaction, which it rolls
        /// back if it is left unused for some minutes. There is none from then on.</summary>
        public event Action TransactionLost;

        // The transaction every read and write is made in, if one has been begun.
        private object transaction;

        public bool InTransaction => transaction != null;

        /// <summary>Begins a transaction: from now on writes are kept to this connection, and
        /// its reads see them, until <see cref="End"/>. False if the server refuses.</summary>
        public async Task<bool> Begin()
        {
            var reply = await Send(HttpMethod.Post, "transactions", null).ConfigureAwait(false);
            if (reply.Ok)
                transaction = Json.Field(reply.Body, "transaction");
            return reply.Ok;
        }

        /// <summary>Ends the transaction, storing its writes for everyone or discarding them.
        /// Null if that was done, otherwise why it was not.</summary>
        public async Task<string> End(bool commit)
        {
            var path = "transactions/" + transaction + (commit ? "/commit" : "/rollback");
            var reply = await Send(HttpMethod.Post, path, null).ConfigureAwait(false);
            // Whatever the answer, a transaction the server has ended or lost is over.
            if (reply.Ok || reply.Status == 404)
                transaction = null;
            return reply.Ok ? null
                : Json.Field(reply.Body, "message") as string ?? "the server could not be reached";
        }

        /// <summary>Whether an answer says the server no longer knows the transaction; if so
        /// there is none from here on, and whoever listens is told.</summary>
        private bool Lost(object error)
        {
            if (transaction == null || Json.Field(error, "code") as string != "unknown_transaction")
                return false;
            transaction = null;
            TransactionLost?.Invoke();
            return true;
        }

        /// <summary>How many requests have been sent; the tests count them.</summary>
        public int Requests;

        public Connection(string address)
        {
            Address = address.TrimEnd('/');
            http = new HttpClient { BaseAddress = new Uri(Address + "/") };
            timer = new Timer(_ => { var sent = Flush(); });
        }

        public void Dispose()
        {
            timer.Dispose();
            http.Dispose();
        }

        /// <summary>The value of a cell: a number, or text for a string cell.</summary>
        public Task<object> Read(string cube, string[] at)
        {
            return Queue(new Cell { Cube = cube, At = at });
        }

        /// <summary>Writes a number or text to a cell, and answers with what was written.</summary>
        public Task<object> Write(string cube, string[] at, object value)
        {
            return Queue(new Cell { Cube = cube, At = at, Value = value });
        }

        /// <summary>Answers a question about a dimension from the copy held here, fetching it
        /// if this is the first question about it.</summary>
        public async Task<object> Ask(string dimension, Func<Dimension, object> question)
        {
            var key = Dimension.Key(dimension);
            Task<Reply> fetch;
            lock (gate)
            {
                if (!dimensions.TryGetValue(key, out fetch))
                {
                    fetch = Send(HttpMethod.Get, "dimensions/" + Uri.EscapeDataString(dimension), null);
                    dimensions[key] = fetch;
                }
            }
            var reply = await fetch.ConfigureAwait(false);
            if (reply.Ok)
                return question(new Dimension(reply.Body));
            if (reply.Status == 404)
                return KeyError;
            // A failure is not worth remembering; the next question asks the server again.
            lock (gate)
            {
                if (dimensions.TryGetValue(key, out var kept) && kept == fetch)
                    dimensions.Remove(key);
            }
            return Unavailable;
        }

        /// <summary>Forgets the dimensions held, so that the next questions fetch them again.</summary>
        public void Refresh()
        {
            lock (gate)
                dimensions = new Dictionary<string, Task<Reply>>();
        }

        private const int BusyRetries = 10;
        private static readonly TimeSpan BusyWait = TimeSpan.FromSeconds(1);

        private static bool Busy(Reply reply)
        {
            return reply.Status == 429 && Json.Field(reply.Body, "code") as string == "busy";
        }

        /// <summary>Starts a session. True if the server accepted the name and password, or
        /// asks for no sign-in at all.</summary>
        public async Task<bool> SignIn(string name, string password)
        {
            var credentials = new Dictionary<string, object> { ["name"] = name, ["password"] = password };
            var reply = await Send(HttpMethod.Post, "sessions", credentials, quietly: true).ConfigureAwait(false);
            // A server checking a crowd of passwords says it is busy and to come back in a
            // second. That is no refusal of this name, so it is asked again, a few times.
            for (var again = 0; again < BusyRetries && Busy(reply); again++)
            {
                await Task.Delay(BusyWait).ConfigureAwait(false);
                reply = await Send(HttpMethod.Post, "sessions", credentials, quietly: true).ConfigureAwait(false);
            }
            if (reply.Ok)
                http.DefaultRequestHeaders.Authorization =
                    new AuthenticationHeaderValue("Bearer", (string)Json.Field(reply.Body, "token"));
            return reply.Ok || reply.Status == 404;
        }

        /// <summary>Whether a session has been begun and not ended here. The server may have
        /// ended it since, which the next request finds out.</summary>
        public bool SignedIn => http.DefaultRequestHeaders.Authorization != null;

        /// <summary>Ends the session, if there is one.</summary>
        public async Task SignOut()
        {
            if (http.DefaultRequestHeaders.Authorization == null)
                return;
            await Send(HttpMethod.Delete, "sessions", null, quietly: true).ConfigureAwait(false);
            http.DefaultRequestHeaders.Authorization = null;
        }

        /// <summary>Shares a value out among the leaves of a consolidated cell, by one of the
        /// server's methods: proportional, equal or repeat. Answers with the value, or with
        /// <see cref="KeyError"/> if the server refuses the spread.</summary>
        public async Task<object> Spread(string cube, string[] at, double value, string method)
        {
            var spread = new Dictionary<string, object>
            {
                ["cube"] = cube, ["at"] = at, ["value"] = value, ["method"] = method,
            };
            var body = Within(new Dictionary<string, object> { ["spread"] = spread });
            var reply = await Send(HttpMethod.Post, "cells/spread", body).ConfigureAwait(false);
            Lost(reply.Body);
            return reply.Ok ? value : reply.Status == 422 || reply.Status == 403 ? KeyError : Unavailable;
        }

        private Task<object> Queue(Cell cell)
        {
            lock (gate)
            {
                (cell.Value == null ? reads : writes).Add(cell);
                // Each new cell puts the sending off a little, so that one calculation of a
                // workbook goes out together.
                timer.Change(Gather, Timeout.InfiniteTimeSpan);
            }
            return cell.Answer.Task;
        }

        /// <summary>Sends everything asked for so far: the writes, then the reads, so that a
        /// read sees a write made in the same calculation.</summary>
        public async Task Flush()
        {
            await sending.WaitAsync().ConfigureAwait(false);
            try
            {
                List<Cell> reading, writing;
                lock (gate)
                {
                    reading = reads;
                    writing = writes;
                    reads = new List<Cell>();
                    writes = new List<Cell>();
                }
                await SendWrites(writing).ConfigureAwait(false);
                await SendReads(reading).ConfigureAwait(false);
            }
            finally
            {
                sending.Release();
            }
        }

        /// <summary>Writes the cells together. A cell the server refuses is answered with
        /// <see cref="KeyError"/> and left out, and the others are tried again.</summary>
        private async Task SendWrites(List<Cell> cells)
        {
            while (cells.Count > 0)
            {
                var reply = await Send(HttpMethod.Post, "cells/write", Body(cells, true)).ConfigureAwait(false);
                var refused = Json.Field(reply.Body, "index");
                // Meant to be held back in a transaction that is gone, the writes are not
                // made at all: made now, they would be final.
                Lost(reply.Body);
                // 422 and 403 with a position refuse the one write there: it is invalid, or it
                // is to a cube the user may not write.
                if ((reply.Status != 422 && reply.Status != 403) || refused == null)
                {
                    // 403: the user may read but not write, so every one of them is refused.
                    var failed = reply.Status == 403 ? KeyError : Unavailable;
                    foreach (var cell in cells)
                        cell.Answer.SetResult(reply.Ok ? cell.Value : failed);
                    return;
                }
                var index = Convert.ToInt32(refused);
                cells[index].Answer.SetResult(KeyError);
                cells.RemoveAt(index);
            }
        }

        private async Task SendReads(List<Cell> cells)
        {
            if (cells.Count == 0)
                return;
            var reply = await Send(HttpMethod.Post, "cells/read", Body(cells, false)).ConfigureAwait(false);
            var results = Json.Items(Json.Field(reply.Body, "cells"));
            // Read in a transaction that is gone, every cell says so; read again without it.
            if (results.Length > 0 && Lost(Json.Field(results[0], "error")))
            {
                reply = await Send(HttpMethod.Post, "cells/read", Body(cells, false)).ConfigureAwait(false);
                results = Json.Items(Json.Field(reply.Body, "cells"));
            }
            for (var index = 0; index < cells.Count; index++)
            {
                var value = index < results.Length ? Json.Field(results[index], "value") : null;
                var answer = !reply.Ok ? Unavailable : value == null ? KeyError : Json.Scalar(value);
                cells[index].Answer.SetResult(answer);
            }
        }

        /// <summary>A request body, made part of the transaction if there is one.</summary>
        private Dictionary<string, object> Within(Dictionary<string, object> body)
        {
            if (transaction != null)
                body["transaction"] = transaction;
            return body;
        }

        private object Body(List<Cell> cells, bool values)
        {
            var items = cells.Select(cell =>
            {
                var item = new Dictionary<string, object> { ["cube"] = cell.Cube, ["at"] = cell.At };
                if (values)
                    item["value"] = cell.Value;
                return item;
            });
            return Within(new Dictionary<string, object> { ["cells"] = items.ToArray() });
        }

        /// <summary>Sends one request. A server that cannot be reached gives status zero.</summary>
        private async Task<Reply> Send(HttpMethod method, string path, object body, bool quietly = false)
        {
            Interlocked.Increment(ref Requests);
            var reply = new Reply();
            try
            {
                var request = new HttpRequestMessage(method, path);
                if (body != null)
                    request.Content = new StringContent(Json.Write(body), Encoding.UTF8, "application/json");
                using (var response = await http.SendAsync(request).ConfigureAwait(false))
                {
                    var text = await response.Content.ReadAsStringAsync().ConfigureAwait(false);
                    reply.Status = (int)response.StatusCode;
                    reply.Body = text == "" ? null : Json.Read(text);
                    if (response.Headers.TryGetValues("Epiphany-Model-Version", out var versions))
                    {
                        var before = Interlocked.Exchange(ref version, versions.First());
                        if (before != null && before != version)
                        {
                            Refresh();
                            ModelChanged?.Invoke();
                        }
                    }
                }
            }
            catch (Exception error) when (error is HttpRequestException || error is OperationCanceledException
                || error is ArgumentException || error is InvalidOperationException)
            {
                // Unreachable, timed out, or an answer that is not JSON: no server to speak of.
                reply.Status = 0;
            }
            if (reply.Status == 401 && !quietly)
                SignInNeeded?.Invoke();
            return reply;
        }
    }
}
