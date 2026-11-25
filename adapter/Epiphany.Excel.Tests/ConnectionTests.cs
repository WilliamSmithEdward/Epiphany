using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Xunit;

namespace Epiphany.Excel.Tests
{
    public sealed class ConnectionTests : IClassFixture<Home>, IDisposable
    {
        private const string Ledger = "General Ledger";
        private readonly Connection connection;

        public ConnectionTests(Home home)
        {
            connection = new Connection(home.Address);
        }

        public void Dispose()
        {
            connection.Dispose();
        }

        private static string[] At(params string[] elements)
        {
            return elements;
        }

        [Fact]
        public async Task ReadsAskedForTogetherTravelAsOneRequest()
        {
            var months = new[] { "Jan", "Feb", "Mar", "Apr", "May", "Jun" };
            var reads = months.Select(month => connection.Read(Ledger, At("Rent", "UK", month, "Budget")));
            var values = await Task.WhenAll(reads.ToArray());
            // An empty numeric cell reads as zero.
            Assert.All(values, value => Assert.Equal(0.0, value));
            Assert.Equal(1, connection.Requests);
        }

        [Fact]
        public async Task ManyReadsTravelAFewThousandAtATime()
        {
            // As many as a large workbook asks for in one calculation.
            var months = new[] { "Jan", "Feb", "Mar", "Apr", "May", "Jun" };
            var reads = Enumerable.Range(0, 12000)
                .Select(index => connection.Read(Ledger, At("Rent", "UK", months[index % 6], "Budget")));
            var values = await Task.WhenAll(reads.ToArray());
            Assert.Equal(12000, values.Length);
            // Every one is answered with a number, and the same cell with the same one.
            Assert.All(values, value => Assert.IsType<double>(value));
            Assert.All(Enumerable.Range(6, 11994), index => Assert.Equal(values[index % 6], values[index]));
            // At five thousand a request, no fewer than three.
            Assert.True(connection.Requests >= 3, connection.Requests + " requests");
        }

        [Fact]
        public async Task AReadSeesAWriteOfTheSameCalculation()
        {
            // Asked for in the order a worksheet might calculate them: the read first.
            var total = connection.Read(Ledger, At("Revenue", "Total Entity", "Q1", "Actual"));
            var first = connection.Write(Ledger, At("Product Revenue", "US", "Jan", "Actual"), 100.0);
            // Elements can be given by alias, and names ignore case and spaces.
            var second = connection.Write(Ledger, At("servicerevenue", "uk", "M02", "ACTUAL"), 50.5);
            Assert.Equal(100.0, await first);
            Assert.Equal(50.5, await second);
            Assert.Equal(150.5, await total);
            Assert.Equal(2, connection.Requests);
        }

        [Fact]
        public async Task ARefusedWriteDoesNotStopTheOthers()
        {
            var consolidated = connection.Write(Ledger, At("Revenue", "US", "Mar", "Actual"), 1.0);
            var leaf = connection.Write(Ledger, At("Salaries", "US", "Mar", "Actual"), 7.0);
            var unknown = connection.Write(Ledger, At("Salaries", "Mars", "Mar", "Actual"), 2.0);
            var text = connection.Write(Ledger, At("Salaries", "UK", "Mar", "Actual"), "seven");
            Assert.Equal(Connection.KeyError, await consolidated);
            Assert.Equal(Connection.KeyError, await unknown);
            Assert.Equal(Connection.KeyError, await text);
            Assert.Equal(7.0, await leaf);
            Assert.Equal(7.0, await connection.Read(Ledger, At("Salaries", "US", "Mar", "Actual")));
            Assert.Equal(0.0, await connection.Read(Ledger, At("Salaries", "UK", "Mar", "Actual")));
        }

        [Fact]
        public async Task AValueIsSpreadOverAConsolidation()
        {
            Func<string, string, string[]> at = (account, entity) => At(account, entity, "Apr", "Budget");
            await connection.Write(Ledger, at("Salaries", "US"), 10.0);
            await connection.Write(Ledger, at("Salaries", "UK"), 30.0);
            // In proportion to what the leaves hold.
            Assert.Equal(80.0, await connection.Spread(Ledger, at("Salaries", "Total Entity"), 80.0, "proportional"));
            Assert.Equal(20.0, await connection.Read(Ledger, at("Salaries", "US")));
            Assert.Equal(60.0, await connection.Read(Ledger, at("Salaries", "UK")));
            // The same to each, or the value itself to each.
            Assert.Equal(50.0, await connection.Spread(Ledger, at("Rent", "Total Entity"), 50.0, "equal"));
            Assert.Equal(25.0, await connection.Read(Ledger, at("Rent", "UK")));
            Assert.Equal(7.0, await connection.Spread(Ledger, at("Rent", "Total Entity"), 7.0, "repeat"));
            Assert.Equal(14.0, await connection.Read(Ledger, at("Rent", "Total Entity")));
            // What the server refuses is a key error, as for a write.
            Assert.Equal(Connection.KeyError, await connection.Spread(Ledger, at("Rent", "Mars"), 1.0, "equal"));
            Assert.Equal(Connection.KeyError, await connection.Spread(Ledger, at("Rent", "UK"), 1.0, "sideways"));
            Assert.Equal(Connection.KeyError, await connection.Spread("Notes", At("Total Entity", "Comment"), 1.0, "equal"));
        }

        [Fact]
        public async Task ATransactionKeepsItsWritesUntilItIsCommitted()
        {
            var at = At("Rent", "US", "May", "Budget");
            using (var other = new Connection(connection.Address))
            {
                Assert.False(connection.InTransaction);
                Assert.True(await connection.Begin());
                Assert.True(connection.InTransaction);
                Assert.Equal(9.0, await connection.Write(Ledger, at, 9.0));
                Assert.Equal(9.0, await connection.Read(Ledger, at));
                Assert.Equal(0.0, await other.Read(Ledger, at));
                Assert.Null(await connection.End(commit: true));
                Assert.False(connection.InTransaction);
                Assert.Equal(9.0, await other.Read(Ledger, at));

                // Rolled back, what was written and spread in it never was.
                Assert.True(await connection.Begin());
                Assert.Equal(5.0, await connection.Write(Ledger, at, 5.0));
                Assert.Equal(40.0, await connection.Spread(Ledger, At("Rent", "Total Entity", "May", "Budget"), 40.0, "equal"));
                Assert.Equal(20.0, await connection.Read(Ledger, at));
                Assert.Null(await connection.End(commit: false));
                Assert.Equal(9.0, await connection.Read(Ledger, at));
            }
        }

        [Fact]
        public async Task ATransactionTheServerHasEndedIsNoticed()
        {
            var at = At("Salaries", "US", "May", "Budget");
            var lost = 0;
            connection.TransactionLost += () => lost++;
            await connection.Write(Ledger, at, 3.0);

            // The server rolls back a transaction left unused; here it is ended behind the
            // connection's back, which comes to the same.
            async Task Vanish()
            {
                Assert.True(await connection.Begin());
                var field = typeof(Connection).GetField("transaction",
                    System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance);
                using (var http = new System.Net.Http.HttpClient())
                {
                    var path = connection.Address + "/transactions/" + field.GetValue(connection) + "/rollback";
                    Assert.True((await http.PostAsync(path, null)).IsSuccessStatusCode);
                }
            }

            // A read is answered all the same, from what is stored.
            await Vanish();
            Assert.Equal(3.0, await connection.Read(Ledger, at));
            Assert.Equal(1, lost);
            Assert.False(connection.InTransaction);

            // A write meant to be held back is not made at all.
            await Vanish();
            Assert.Same(Connection.Unavailable, await connection.Write(Ledger, at, 8.0));
            Assert.Equal(2, lost);
            Assert.Equal(3.0, await connection.Read(Ledger, at));

            // And ending it says why it could not be.
            await Vanish();
            Assert.Contains("unknown transaction", await connection.End(commit: true));
            Assert.False(connection.InTransaction);
        }

        [Fact]
        public async Task WhatCannotBeResolvedIsAKeyError()
        {
            var cube = connection.Read("No Such Cube", At("Rent", "UK", "Jan", "Actual"));
            var element = connection.Read(Ledger, At("Rent", "UK", "Smarch", "Actual"));
            var arity = connection.Read(Ledger, At("Rent", "UK"));
            var good = connection.Read(Ledger, At("Rent", "UK", "Jan", "Variance"));
            Assert.Equal(Connection.KeyError, await cube);
            Assert.Equal(Connection.KeyError, await element);
            Assert.Equal(Connection.KeyError, await arity);
            Assert.Equal(0.0, await good);
        }

        [Fact]
        public async Task StringCellsHoldText()
        {
            Assert.Equal("", await connection.Read("Notes", At("US", "Comment")));
            Assert.Equal("late", await connection.Write("Notes", At("US", "Comment"), "late"));
            Assert.Equal("late", await connection.Read("Notes", At("US", "Comment")));
            Assert.Equal(Connection.KeyError, await connection.Write("Notes", At("UK", "Comment"), 3.0));
            // The numeric element of the same cube, by its alias.
            Assert.Equal(3.0, await connection.Write("Notes", At("UK", "Amt"), 3.0));
            Assert.Equal(3.0, await connection.Read("Notes", At("Total Entity", "Amount")));
        }

        [Fact]
        public async Task DimensionsAreFetchedOnceUntilRefreshed()
        {
            Assert.Equal(9.0, await connection.Ask("Period", period => period.Size()));
            Assert.Equal("Q1", await connection.Ask("period", period => period.Name(2, "")));
            Assert.Equal(1, connection.Requests);
            connection.Refresh();
            Assert.Equal(4.0, await connection.Ask("PERIOD", period => period.Index("Jan")));
            Assert.Equal(2, connection.Requests);
            Assert.Equal(Connection.KeyError, await connection.Ask("Planet", planet => planet.Size()));
        }

        [Fact]
        public async Task DimensionFunctionsTakeWhateverAWorksheetHandsThem()
        {
            object Period(Func<Dimension, object> question) => connection.Ask("Period", question).Result;
            await connection.Ask("Period", period => period.Size());

            // A position that is no position is past the end: empty text, as beyond the last.
            foreach (var index in new[] { double.NaN, double.NegativeInfinity, double.PositiveInfinity, -1, 0.99, 1e300 })
            {
                Assert.Equal("", Period(period => period.Name(index, "")));
                Assert.Equal("", Period(period => period.Parent("Jan", index)));
                Assert.Equal("", Period(period => period.Child("Q1", index)));
                Assert.Equal("", Period(period => period.SubsetName("", index, "")));
            }
            // A fraction is the position it has passed.
            Assert.Equal("H1", Period(period => period.Name(1.9, "")));
            // A name that is none, empty, or not text at all is no element.
            foreach (var name in new[] { null, "", "   ", "nobody", "Jan,Feb", "\0" })
            {
                Assert.Equal(0.0, Period(period => period.Index(name)));
                Assert.Equal(Connection.KeyError, Period(period => period.Level(name)));
                Assert.Equal(Connection.KeyError, Period(period => period.Next(name)));
                Assert.Equal(Connection.KeyError, Period(period => period.IsChild(name, "Q1")));
                Assert.Equal(Connection.KeyError, Period(period => period.IsAncestor("H1", name)));
                Assert.Equal(Connection.KeyError, Period(period => period.Weight(name, name)));
                Assert.Equal(Connection.KeyError, Period(period => period.Attribute(name, "Code")));
                Assert.Equal(Connection.KeyError, Period(period => period.Attribute("Jan", name)));
                Assert.Equal("", Period(period => period.SubsetName("", name, "")));
            }
            Assert.Equal("", Period(period => period.SubsetName("", true, "")));
            Assert.Equal(Connection.KeyError, Period(period => period.SubsetName("nobody", 1.0, "")));
            Assert.Equal(Connection.KeyError, Period(period => period.Members("", "nobody")));
            Assert.Equal(Connection.KeyError, Period(period => period.SubsetSize(null)));
        }

        [Fact]
        public async Task DimensionFunctionsAnswerFromTheHierarchy()
        {
            object Period(Func<Dimension, object> question) => connection.Ask("Period", question).Result;
            await connection.Ask("Period", period => period.Size());

            Assert.Equal("H1", Period(period => period.Name(1, "")));
            Assert.Equal("", Period(period => period.Name(0, "")));
            Assert.Equal("", Period(period => period.Name(10, "")));
            Assert.Equal("M01", Period(period => period.Name(4, "Code")));
            // A consolidation has no alias here, so it shows its name.
            Assert.Equal("Q1", Period(period => period.Name(2, "code")));
            Assert.Equal(Connection.KeyError, Period(period => period.Name(2, "Days")));

            Assert.Equal(4.0, Period(period => period.Index("jan")));
            Assert.Equal(4.0, Period(period => period.Index("M01")));
            Assert.Equal(0.0, Period(period => period.Index("Smarch")));

            Assert.Equal(0.0, Period(period => period.Level("Jan")));
            Assert.Equal(1.0, Period(period => period.Level("Q1")));
            Assert.Equal(2.0, Period(period => period.Level("H1")));
            Assert.Equal(Connection.KeyError, Period(period => period.Level("Smarch")));

            Assert.Equal("Q1", Period(period => period.Parent("Feb", 1)));
            Assert.Equal("", Period(period => period.Parent("Feb", 2)));
            Assert.Equal("", Period(period => period.Parent("H1", 1)));
            Assert.Equal(1.0, Period(period => period.ParentCount("Feb")));
            Assert.Equal("May", Period(period => period.Child("Q2", 2)));
            Assert.Equal("", Period(period => period.Child("Q2", 4)));
            Assert.Equal("", Period(period => period.Child("May", 1)));
            Assert.Equal(3.0, Period(period => period.ChildCount("Q2")));
            Assert.Equal(Connection.KeyError, Period(period => period.Child("Smarch", 1)));
        }

        [Fact]
        public async Task ElementsAreToldApartAndRelated()
        {
            object Period(Func<Dimension, object> question) => connection.Ask("Period", question).Result;
            await connection.Ask("Period", period => period.Size());

            Assert.Equal(3.0, Period(period => period.Levels()));
            Assert.Equal("Q1", Period(period => period.Next("H1")));
            Assert.Equal("", Period(period => period.Next("Jun")));
            Assert.Equal(Connection.KeyError, Period(period => period.Next("Smarch")));
            Assert.Equal("C", Period(period => period.Type("Q1")));
            Assert.Equal("N", Period(period => period.Type("M01")));
            Assert.Equal("S", await connection.Ask("Line", line => line.Type("Comment")));

            Assert.Equal(true, Period(period => period.IsChild("Jan", "Q1")));
            Assert.Equal(false, Period(period => period.IsChild("Jan", "H1")));
            Assert.Equal(false, Period(period => period.IsChild("Q1", "Jan")));
            Assert.Equal(true, Period(period => period.IsAncestor("H1", "jan")));
            Assert.Equal(true, Period(period => period.IsAncestor("Q1", "Jan")));
            Assert.Equal(false, Period(period => period.IsAncestor("Jan", "H1")));
            Assert.Equal(false, Period(period => period.IsAncestor("Q2", "Jan")));
            Assert.Equal(Connection.KeyError, Period(period => period.IsChild("Jan", "Smarch")));

            Assert.Equal(-1.0, await connection.Ask("Account", a => a.Weight("Net Income", "Operating Expenses")));
            Assert.Equal(1.0, await connection.Ask("Account", a => a.Weight("Net Income", "Revenue")));
            // Not a child of it, though beneath it.
            Assert.Equal(0.0, await connection.Ask("Account", a => a.Weight("Net Income", "Rent")));
            Assert.Equal(2.0, await connection.Ask("Line", line => line.SubsetSize("backwards")));
            Assert.Equal(Connection.KeyError, await connection.Ask("Line", line => line.SubsetSize("Nope")));
        }

        [Fact]
        public async Task AttributesReadAsTextOrNumbers()
        {
            object Period(Func<Dimension, object> question) => connection.Ask("Period", question).Result;
            await connection.Ask("Period", period => period.Size());

            Assert.Equal(28.0, Period(period => period.Attribute("Feb", "Days")));
            Assert.Equal("M02", Period(period => period.Attribute("Feb", "Code")));
            // Without a value: zero, or for an alias the element's name.
            Assert.Equal(0.0, Period(period => period.Attribute("Q1", "Days")));
            Assert.Equal("Q1", Period(period => period.Attribute("Q1", "Code")));
            Assert.Equal(Connection.KeyError, Period(period => period.Attribute("Feb", "Colour")));
            Assert.Equal(Connection.KeyError, Period(period => period.Attribute("Smarch", "Days")));

            Assert.Equal("28", Period(period => period.AttributeText("Feb", "Days")));
            Assert.Equal("M02", Period(period => period.AttributeText("Feb", "Code")));
            Assert.Equal(28.0, Period(period => period.AttributeNumber("Feb", "Days")));
            Assert.Equal(Connection.KeyError, Period(period => period.AttributeNumber("Feb", "Code")));

            Assert.Equal("Income", await connection.Ask("Account", a => a.Attribute("Product Revenue", "AccountType")));
            Assert.Equal("", await connection.Ask("Account", a => a.Attribute("Revenue", "AccountType")));
        }

        [Fact]
        public async Task SubsetsSelectByIndexOrByName()
        {
            object Line(Func<Dimension, object> question) => connection.Ask("Line", question).Result;
            await connection.Ask("Line", line => line.Size());

            Assert.Equal("Amount", Line(line => line.SubsetName("Backwards", 1.0, "")));
            Assert.Equal("Comment", Line(line => line.SubsetName("backwards", 2.0, "")));
            Assert.Equal("", Line(line => line.SubsetName("Backwards", 3.0, "")));
            Assert.Equal("Amt", Line(line => line.SubsetName("Backwards", 1.0, "Short")));
            // A named element is given back if the subset holds it, whatever it was called by.
            Assert.Equal("Comment", Line(line => line.SubsetName("Texts", "comment", "")));
            Assert.Equal("", Line(line => line.SubsetName("Texts", "Amount", "")));
            Assert.Equal("Amount", Line(line => line.SubsetName("Backwards", "Amt", "")));
            // No subset name means the whole dimension, in its own order.
            Assert.Equal("Comment", Line(line => line.SubsetName("", 1.0, "")));
            Assert.Equal("Amt", Line(line => line.SubsetName("", "amount", "Short")));
            Assert.Equal(Connection.KeyError, Line(line => line.SubsetName("No Such Subset", 1.0, "")));
        }

        [Fact]
        public async Task ASubsetListsItsElementsForChoosing()
        {
            var names = await connection.Ask("Line", line => line.Members("Backwards", ""));
            Assert.Equal(
                new[] { "Amount=Amount", "Comment=Comment" },
                ((List<KeyValuePair<string, string>>)names).Select(member => member.Key + "=" + member.Value).ToArray());
            // With an alias, each element is shown by it, or by its name where it has none.
            var aliases = await connection.Ask("Line", line => line.Members("", "Short"));
            Assert.Equal(
                new[] { "Comment=Comment", "Amount=Amt" },
                ((List<KeyValuePair<string, string>>)aliases).Select(member => member.Key + "=" + member.Value).ToArray());
            Assert.Equal(Connection.KeyError, await connection.Ask("Line", line => line.Members("Nope", "")));
            Assert.Equal(Connection.KeyError, await connection.Ask("Line", line => line.Members("", "Nope")));
        }

        [Fact]
        public void NoFormulaPutTogetherAtRandomIsMoreThanTheReaderCanTake()
        {
            // Pieces of formulas, strung together any way: read as a call or not, never an error.
            var pieces = new[]
            {
                "=", "DBRW", "SUBNM", "(", ")", ",", "\"", "'", "{", "}", " ", "A1", "$B$2", "\"a,b\"",
                "'My sheet'!", "1", "+", "*", "IF(", "\"\"", "''", "é", "\n", "((", "))", "=DBRW(", ".", "_",
            };
            var random = new Random(7);
            for (var attempt = 0; attempt < 50000; attempt++)
            {
                var formula = string.Concat(Enumerable.Range(0, random.Next(12)).Select(_ => pieces[random.Next(pieces.Length)]));
                var call = Formula.Call(formula);
                if (call == null)
                    continue;
                // What is read as a call has a function, and written out again reads the same.
                Assert.NotEqual("", call[0]);
                Assert.Equal(call, Formula.Call(Formula.With(call, 0, call[0])));
                Assert.NotNull(Formula.With(call, call.Length + 2, Formula.Quote(formula)));
            }
        }

        [Fact]
        public void AFormulaIsReadAsOneCallOrNotAtAll()
        {
            Assert.Equal(new[] { "SUBNM", "\"Period\"", "\"\"", "1" }, Formula.Call("=SUBNM(\"Period\",\"\",1)"));
            // Commas and brackets inside text or inside another call belong to the argument.
            Assert.Equal(
                new[] { "DBRW", "\"a,b)\"", "IF(A1>0,B1,C1)", "{1,2}", "$D$4" },
                Formula.Call("=dbrw( \"a,b)\" , IF(A1>0,B1,C1), {1,2}, $D$4)"));
            Assert.Equal(new[] { "NOW" }, Formula.Call("=NOW()"));
            Assert.Null(Formula.Call("=DBRW(\"a\",\"b\")*2"));
            Assert.Null(Formula.Call("=A1+DBRW(\"a\",\"b\")"));
            Assert.Null(Formula.Call("12"));
            Assert.Null(Formula.Call("=DBRW(\"a\""));

            // So do those in the name of a sheet, which stands in single quotes, an apostrophe
            // in it doubled; and an apostrophe in text is no such quote.
            Assert.Equal(
                new[] { "DBRW", "'Q1, plan (old'!A1", "'It''s, here'!B2", "\"it's, text\"" },
                Formula.Call("=DBRW('Q1, plan (old'!A1,'It''s, here'!B2,\"it's, text\")"));

            var call = Formula.Call("=SUBNM(\"Period\",\"Months\",1,\"Code\")");
            Assert.Equal("=SUBNM(\"Period\",\"Months\",\"Mar\",\"Code\")", Formula.With(call, 3, Formula.Quote("Mar")));
            Assert.Equal("=SUBNM(\"Period\",\"\",\"5\"\" disk\")",
                Formula.With(Formula.Call("=SUBNM(\"Period\",\"\")"), 3, Formula.Quote("5\" disk")));
        }

        [Fact]
        public async Task AServerThatIsNotThereIsUnavailable()
        {
            // Port one on this machine: nothing listens there.
            using (var nowhere = new Connection("http://127.0.0.1:1"))
            {
                Assert.Same(Connection.Unavailable, await nowhere.Read(Ledger, At("Rent", "UK", "Jan", "Actual")));
                Assert.Same(Connection.Unavailable, await nowhere.Write(Ledger, At("Rent", "UK", "Jan", "Actual"), 1.0));
                Assert.Same(Connection.Unavailable, await nowhere.Ask("Period", period => period.Size()));
                Assert.False(await nowhere.SignIn(Home.User, Home.Password));
            }
        }

        [Fact]
        public async Task SigningInAsksAgainWhenTheServerIsBusy()
        {
            // A stand-in for the server: busy the first time it is asked, then willing.
            var listener = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
            listener.Start();
            var port = ((System.Net.IPEndPoint)listener.LocalEndpoint).Port;
            var asked = 0;
            var serving = Task.Run(async () =>
            {
                while (asked < 2)
                {
                    using (var client = await listener.AcceptTcpClientAsync())
                    {
                        var stream = client.GetStream();
                        var buffer = new byte[8192];
                        var request = "";
                        // The head, and then as much body as it says there is.
                        while (!Whole(request))
                            request += System.Text.Encoding.ASCII.GetString(buffer, 0, await stream.ReadAsync(buffer, 0, buffer.Length));
                        asked++;
                        var body = asked == 1
                            ? "{\"code\":\"busy\",\"message\":\"too many sign-ins are waiting\"}"
                            : "{\"token\":\"abc\",\"idle_seconds\":60,\"role\":\"admin\",\"cubes\":{}}";
                        var status = asked == 1 ? "429 Too Many Requests" : "201 Created";
                        var reply = "HTTP/1.1 " + status + "\r\nContent-Type: application/json\r\nContent-Length: "
                            + body.Length + "\r\nConnection: close\r\n\r\n" + body;
                        var bytes = System.Text.Encoding.ASCII.GetBytes(reply);
                        await stream.WriteAsync(bytes, 0, bytes.Length);
                    }
                }
            });
            using (var busy = new Connection("http://127.0.0.1:" + port))
            {
                Assert.True(await busy.SignIn(Home.User, Home.Password));
                Assert.True(busy.SignedIn);
            }
            await serving;
            listener.Stop();
            Assert.Equal(2, asked);
        }

        /// <summary>Whether the text of a request holds its head and all of its body.</summary>
        private static bool Whole(string request)
        {
            var head = request.IndexOf("\r\n\r\n", StringComparison.Ordinal);
            if (head < 0)
                return false;
            var length = request.Substring(0, head).Split('\n')
                .Where(line => line.StartsWith("Content-Length:", StringComparison.OrdinalIgnoreCase))
                .Select(line => int.Parse(line.Substring(15).Trim()))
                .FirstOrDefault();
            return request.Length >= head + 4 + length;
        }

        [Fact]
        public async Task AServerWithoutUsersNeedsNoSignIn()
        {
            Assert.True(await connection.SignIn(Home.User, Home.Password));
            Assert.Equal(0.0, await connection.Read(Ledger, At("Rent", "UK", "Jun", "Budget")));
        }
    }

    public sealed class SignInTests : IClassFixture<SecuredHome>
    {
        private readonly SecuredHome home;

        public SignInTests(SecuredHome home)
        {
            this.home = home;
        }

        [Fact]
        public async Task AUserWhoMayOnlyReadIsRefusedAWrite()
        {
            var at = new[] { "Rent", "UK", "Feb", "Actual" };
            using (var connection = new Connection(home.Address))
            {
                var asked = 0;
                connection.SignInNeeded += () => asked++;
                Assert.True(await connection.SignIn(Home.Reader, Home.Password));
                Assert.Equal(0.0, await connection.Read("General Ledger", at));
                // Refused, as a write to a consolidation is; the session itself is good.
                Assert.Equal(Connection.KeyError, await connection.Write("General Ledger", at, 5.0));
                Assert.Equal(0.0, await connection.Read("General Ledger", at));
                Assert.Equal(0, asked);
            }
        }

        [Fact]
        public async Task AChangedModelIsNoticedWithoutBeingAskedAbout()
        {
            var at = new[] { "Rent", "UK", "Mar", "Actual" };
            using (var connection = new Connection(home.Address))
            using (var http = new System.Net.Http.HttpClient())
            {
                var changes = 0;
                connection.ModelChanged += () => changes++;
                Assert.True(await connection.SignIn(Home.User, Home.Password));
                // A dimension the model does not have, and the answer is held.
                Assert.Equal(Connection.KeyError, await connection.Ask("Extra", extra => extra.Size()));
                Assert.Equal(Connection.KeyError, await connection.Ask("Extra", extra => extra.Size()));
                Assert.Equal(0, changes);

                // Someone else adds it. Nothing tells this connection, until its next answer
                // from the server comes from the new version of the model.
                var token = await SignIn(http);
                http.DefaultRequestHeaders.Add("Authorization", "Bearer " + token);
                var extra = "{\"based_on\":1,\"dimension\":{\"name\":\"Extra\",\"elements\":[{\"name\":\"X\",\"type\":\"numeric\"}]}}";
                var put = new System.Net.Http.StringContent(extra, System.Text.Encoding.UTF8, "application/json");
                var response = await http.PutAsync(home.Address + "/dimensions/Extra", put);
                Assert.True(response.IsSuccessStatusCode);
                Assert.Equal(0, changes);

                Assert.Equal(0.0, await connection.Read("General Ledger", at));
                Assert.Equal(1, changes);
                Assert.Equal(1.0, await connection.Ask("Extra", found => found.Size()));
                Assert.Equal(1, changes);
            }
        }

        private async Task<string> SignIn(System.Net.Http.HttpClient http)
        {
            var credentials = "{\"name\":\"" + Home.User + "\",\"password\":\"" + Home.Password + "\"}";
            var body = new System.Net.Http.StringContent(credentials, System.Text.Encoding.UTF8, "application/json");
            var response = await http.PostAsync(home.Address + "/sessions", body);
            var text = await response.Content.ReadAsStringAsync();
            var from = text.IndexOf("\"token\":\"", StringComparison.Ordinal) + 9;
            return text.Substring(from, text.IndexOf('"', from) - from);
        }

        [Fact]
        public async Task ASessionIsNeededAndCanBeEnded()
        {
            var at = new[] { "Rent", "UK", "Jan", "Actual" };
            using (var connection = new Connection(home.Address))
            {
                var asked = 0;
                connection.SignInNeeded += () => asked++;
                Assert.Same(Connection.Unavailable, await connection.Read("General Ledger", at));
                Assert.Same(Connection.Unavailable, await connection.Write("General Ledger", at, 5.0));
                Assert.Same(Connection.Unavailable, await connection.Ask("Period", period => period.Size()));
                Assert.Equal(3, asked);

                Assert.False(await connection.SignIn(Home.User, "wrong"));
                Assert.Equal(3, asked);
                Assert.True(await connection.SignIn(Home.User, Home.Password));
                Assert.Equal(5.0, await connection.Write("General Ledger", at, 5.0));
                Assert.Equal(5.0, await connection.Read("General Ledger", at));
                // A refusal is not remembered: the dimension is asked for again and now arrives.
                Assert.Equal(9.0, await connection.Ask("Period", period => period.Size()));

                await connection.SignOut();
                Assert.Same(Connection.Unavailable, await connection.Read("General Ledger", at));
                Assert.Equal(4, asked);
            }
        }
    }

    /// <summary>Changes to the model, on a server of their own.</summary>
    public sealed class ModelChangeTests : IClassFixture<Home>
    {
        private readonly Home home;

        public ModelChangeTests(Home home)
        {
            this.home = home;
        }

        [Fact]
        public async Task ADimensionIsFetchedAgainOnlyOnceItHasChanged()
        {
            var at = new[] { "Rent", "UK", "Mar", "Actual" };
            using (var connection = new Connection(home.Address))
            using (var http = new System.Net.Http.HttpClient())
            {
                var changes = 0;
                connection.ModelChanged += () => changes++;
                // A read first, so that the connection knows the model's version as it asks.
                Assert.Equal(0.0, await connection.Read("General Ledger", at));
                Assert.Equal(9.0, await connection.Ask("Period", period => period.Size()));
                Assert.Equal(3.0, await connection.Ask("Entity", entity => entity.Size()));

                // Someone gives Entity an element more; Period stays as it was.
                var entity = "{\"based_on\":1,\"dimension\":{\"name\":\"Entity\",\"elements\":["
                    + "{\"name\":\"Total Entity\",\"type\":\"consolidated\",\"children\":[{\"name\":\"US\"},{\"name\":\"UK\"}]},"
                    + "{\"name\":\"US\",\"type\":\"numeric\"},{\"name\":\"UK\",\"type\":\"numeric\"},{\"name\":\"FR\",\"type\":\"numeric\"}]}}";
                var put = new System.Net.Http.StringContent(entity, System.Text.Encoding.UTF8, "application/json");
                Assert.True((await http.PutAsync(home.Address + "/dimensions/Entity", put)).IsSuccessStatusCode);

                // The next answer comes from the new model, and the change is noticed at once.
                Assert.Equal(0.0, await connection.Read("General Ledger", at));
                Assert.Equal(1, changes);
                // Period is answered from the copy held, asking the server nothing; Entity is
                // fetched again, with FR.
                var sent = connection.Requests;
                Assert.Equal(9.0, await connection.Ask("Period", period => period.Size()));
                Assert.Equal(sent, connection.Requests);
                Assert.Equal(4.0, await connection.Ask("Entity", found => found.Size()));
                Assert.Equal(sent + 1, connection.Requests);
            }
        }
    }
}
