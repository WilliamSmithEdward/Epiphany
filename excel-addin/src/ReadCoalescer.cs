using System.Collections.Concurrent;
using ExcelDna.Integration;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// Read-coalescing layer (ADR-0022 point 3). When a recalc touches many
/// <c>EPIPHANY.READ</c> cells, each cell would otherwise fire its own
/// <c>cells/read</c> POST and park a pool thread waiting on the round trip. This
/// coalescer instead collects the individual reads that land inside a short recalc
/// window and issues ONE <c>cells/read</c> POST per (server, sandbox, cube) batch,
/// then fans the results back to the cells that asked.
///
/// It is driven through Excel-DNA's observable async model
/// (<see cref="ExcelAsyncUtil.Observe"/>): the UDF hands back an
/// <see cref="IExcelObservable"/>, ExcelDna hosts it on its RTD topic, and the
/// value is pushed to the cell when the batch resolves. No calc/pool thread is
/// parked per cell - the coalescer owns exactly one background flush task per
/// window, no matter how many cells are pending.
///
/// Correctness notes:
/// - Each cell is keyed by (server, sandbox, cube, coord) via the Observe
///   identity, so a cell always receives ITS own value; results are matched back
///   to subscribers by the coordinate the server echoes, not by list position.
/// - The <c>cells/read</c> endpoint is all-or-nothing: if any coordinate in the
///   POST is unresolvable the server fails the WHOLE request (422). So a failed
///   batch is retried as per-coordinate single reads, so one bad cell surfaces its
///   own error and every good cell in the same window still resolves.
/// - Values are not cached across windows: every recalc re-subscribes and re-reads
///   through the server, so a genuine change is always reflected (Excel debounces
///   the recalc itself; this layer only batches within one tick).
/// </summary>
internal sealed class ReadCoalescer
{
    /// <summary>
    /// The window, in milliseconds, over which reads are gathered before a batch is
    /// sent. Long enough to sweep up the cells of one recalc, short enough that a
    /// single interactive read still feels immediate.
    /// </summary>
    private const int WindowMs = 50;

    /// <summary>The process-wide coalescer. Reads from every cell funnel through it.</summary>
    internal static readonly ReadCoalescer Instance = new();

    private readonly object _gate = new();

    /// <summary>Pending reads not yet flushed, grouped so one POST serves each bucket.</summary>
    private readonly Dictionary<BatchKey, List<PendingRead>> _pending = new();

    /// <summary>
    /// The scheduled flush for the current window, or null when idle. Explicitly
    /// <see cref="System.Threading.Timer"/> - WinForms is referenced (for the
    /// configurator) so the unqualified name is ambiguous, and its callback runs on
    /// a pool thread, which is what we want here (there is no UI thread to marshal
    /// to during a recalc).
    /// </summary>
    private System.Threading.Timer? _flushTimer;

    private ReadCoalescer() { }

    /// <summary>
    /// Register one cell's read and return the observable Excel-DNA drives. The
    /// value (or a per-cell error) is pushed to the cell when this cell's batch
    /// resolves. <paramref name="client"/> is captured by the caller so a mid-flight
    /// sign-out cannot swap it (mirrors the capture in <see cref="Functions"/>).
    /// </summary>
    public IExcelObservable Subscribe(EpiphanyClient client, string cube, Dictionary<string, string> coord)
        => new ReadObservable(this, client, cube, coord);

    /// <summary>Queue a subscribed read and make sure a flush is scheduled.</summary>
    private void Enqueue(PendingRead read)
    {
        lock (_gate)
        {
            var key = new BatchKey(read.Client, read.Cube);
            if (!_pending.TryGetValue(key, out var list))
            {
                list = new List<PendingRead>();
                _pending[key] = list;
            }
            list.Add(read);
            // One timer serves the whole window: the first pending read in an idle
            // coalescer arms it; later reads in the same window just join the queue.
            _flushTimer ??= new System.Threading.Timer(_ => Flush(), null, WindowMs, Timeout.Infinite);
        }
    }

    /// <summary>
    /// Remove a read that unsubscribed before the flush (Excel dropped/replaced the
    /// topic). Its observer must not be delivered to afterwards.
    /// </summary>
    private void Cancel(PendingRead read)
    {
        lock (_gate)
        {
            var key = new BatchKey(read.Client, read.Cube);
            if (_pending.TryGetValue(key, out var list))
            {
                list.Remove(read);
                if (list.Count == 0) _pending.Remove(key);
            }
        }
    }

    /// <summary>Take every queued read, clearing the queue and disarming the timer.</summary>
    private List<KeyValuePair<BatchKey, List<PendingRead>>> Drain()
    {
        lock (_gate)
        {
            var batches = _pending.ToList();
            _pending.Clear();
            _flushTimer?.Dispose();
            _flushTimer = null;
            return batches;
        }
    }

    /// <summary>
    /// Fire each bucket as one POST on a single background task. Called off the
    /// timer thread; ExcelDna marshals the pushed values back to the sheet.
    /// </summary>
    private void Flush()
    {
        var batches = Drain();
        foreach (var batch in batches)
            _ = SendBatchAsync(batch.Key, batch.Value);
    }

    /// <summary>
    /// Read one bucket. Deliver each subscriber its own value on success; on a batch
    /// failure fall back to per-coordinate reads so one bad coordinate cannot poison
    /// the others (the endpoint fails the whole request on any unresolvable coord).
    /// Only still-live (non-cancelled) subscribers are delivered to.
    /// </summary>
    private static async Task SendBatchAsync(BatchKey key, List<PendingRead> reads)
    {
        var live = reads.Where(r => !r.IsCancelled).ToList();
        if (live.Count == 0) return;

        var coords = live.Select(r => (IDictionary<string, string>)r.Coord).ToList();
        try
        {
            var values = await key.Client.ReadCellsAsync(key.Cube, coords).ConfigureAwait(false);
            // The server returns cells in the order of the coords it was sent, so
            // the i-th value belongs to the i-th live read. (A single cube read
            // shares one snapshot, so distinct coords cannot collapse to one entry.)
            for (int i = 0; i < live.Count; i++)
                live[i].Deliver(Functions.ToCell(i < values.Count ? values[i] : null));
        }
        catch (Exception batchError)
        {
            // The whole batch failed - most often one coordinate is unresolvable and
            // the server rejected the request wholesale. Re-read each coordinate on
            // its own so good cells still get values and only the offending cell
            // surfaces the error.
            if (live.Count == 1)
            {
                live[0].DeliverError(batchError);
                return;
            }
            await FallBackPerCellAsync(key, live).ConfigureAwait(false);
        }
    }

    /// <summary>Read each coordinate individually so one failure is isolated to its cell.</summary>
    private static async Task FallBackPerCellAsync(BatchKey key, List<PendingRead> live)
    {
        foreach (var read in live)
        {
            if (read.IsCancelled) continue;
            try
            {
                var value = await key.Client.ReadCellAsync(key.Cube, read.Coord).ConfigureAwait(false);
                read.Deliver(Functions.ToCell(value));
            }
            catch (Exception cellError)
            {
                read.DeliverError(cellError);
            }
        }
    }

    /// <summary>
    /// A batch bucket: reads sharing a client (hence server + auth + sandbox header)
    /// and a cube collapse into one POST. The client is compared by reference - it
    /// is the single mutable connection captured by the UDF - so a sandbox/server
    /// switch (which replaces or reconfigures the client) never merges reads across
    /// contexts.
    /// </summary>
    private readonly struct BatchKey : IEquatable<BatchKey>
    {
        public EpiphanyClient Client { get; }
        public string Cube { get; }

        public BatchKey(EpiphanyClient client, string cube)
        {
            Client = client;
            Cube = cube;
        }

        public bool Equals(BatchKey other)
            => ReferenceEquals(Client, other.Client)
               && string.Equals(Cube, other.Cube, StringComparison.Ordinal)
               // The sandbox lives on the mutable client; fold it into equality so a
               // change between two reads on the same client instance still splits
               // the buckets and never reads one coord under the wrong sandbox.
               && string.Equals(Client.Sandbox, other.Client.Sandbox, StringComparison.Ordinal);

        public override bool Equals(object? obj) => obj is BatchKey other && Equals(other);

        public override int GetHashCode()
            => HashCode.Combine(
                System.Runtime.CompilerServices.RuntimeHelpers.GetHashCode(Client),
                Cube,
                Client.Sandbox ?? "");
    }

    /// <summary>
    /// One cell's queued read: its coordinate plus the observer to push the result
    /// to. <see cref="_state"/> guards single delivery and reflects unsubscription.
    /// </summary>
    private sealed class PendingRead
    {
        private const int Live = 0, Cancelled = 1, Delivered = 2;
        private int _state = Live;
        private readonly IExcelObserver _observer;

        public EpiphanyClient Client { get; }
        public string Cube { get; }
        public Dictionary<string, string> Coord { get; }

        public PendingRead(EpiphanyClient client, string cube, Dictionary<string, string> coord, IExcelObserver observer)
        {
            Client = client;
            Cube = cube;
            Coord = coord;
            _observer = observer;
        }

        public bool IsCancelled => Volatile.Read(ref _state) == Cancelled;

        /// <summary>Mark unsubscribed; a later delivery for this read is dropped.</summary>
        public void Cancel() => Interlocked.CompareExchange(ref _state, Cancelled, Live);

        /// <summary>Push the value to the cell, exactly once, and complete the topic.</summary>
        public void Deliver(object value)
        {
            if (Interlocked.CompareExchange(ref _state, Delivered, Live) != Live) return;
            _observer.OnNext(value);
            _observer.OnCompleted();
        }

        /// <summary>Push a plain-language error to the cell, exactly once.</summary>
        public void DeliverError(Exception error)
            => Deliver("#EPIPHANY: " + error.Message);
    }

    /// <summary>
    /// The observable Excel-DNA subscribes to for one cell. Subscription enqueues the
    /// read; disposal (Excel dropped or replaced the topic) cancels it so a stale
    /// value is never pushed to a reused cell.
    /// </summary>
    private sealed class ReadObservable : IExcelObservable
    {
        private readonly ReadCoalescer _owner;
        private readonly EpiphanyClient _client;
        private readonly string _cube;
        private readonly Dictionary<string, string> _coord;

        public ReadObservable(ReadCoalescer owner, EpiphanyClient client, string cube, Dictionary<string, string> coord)
        {
            _owner = owner;
            _client = client;
            _cube = cube;
            _coord = coord;
        }

        public IDisposable Subscribe(IExcelObserver observer)
        {
            var read = new PendingRead(_client, _cube, _coord, observer);
            _owner.Enqueue(read);
            return new Unsubscriber(_owner, read);
        }

        private sealed class Unsubscriber : IDisposable
        {
            private readonly ReadCoalescer _owner;
            private readonly PendingRead _read;

            public Unsubscriber(ReadCoalescer owner, PendingRead read)
            {
                _owner = owner;
                _read = read;
            }

            public void Dispose()
            {
                _read.Cancel();
                _owner.Cancel(_read);
            }
        }
    }
}
