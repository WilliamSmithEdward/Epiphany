using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using System.Windows.Forms;
using ExcelDna.Integration;

namespace Epiphany.Excel
{
    /// <summary>The add-in itself: the environments the worksheet functions can name, each a
    /// server with its own session, and the commands of the Epiphany menu.</summary>
    public sealed class AddIn : IExcelAddIn
    {
        private const string DefaultName = "local";
        private const string DefaultAddress = "http://127.0.0.1:8080";
        private const string Menu = "Epiphany";

        private static readonly string SettingsFile = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "Epiphany", "adapter.json");

        /// <summary>A server under the name formulas know it by, as in "production:Sales".</summary>
        private sealed class Place
        {
            public string Name;
            public string User;
            public Connection Connection;
            // The user closed the sign-in window without signing in; do not open it again unasked.
            public bool Declined;
        }

        private static readonly object Gate = new object();
        private static readonly Dictionary<string, Place> Places = new Dictionary<string, Place>();
        // The environment of a formula that names none.
        private static Place fallback;
        /// <summary>How many times Refresh has run.</summary>
        internal static double Refreshes { get; private set; }

        // The sign-in window is open or on its way.
        private static bool asking;

        public void AutoOpen()
        {
            try
            {
                var settings = Json.Read(File.ReadAllText(SettingsFile));
                var saved = Json.Field(settings, "environments") as Dictionary<string, object>;
                foreach (var place in saved ?? new Dictionary<string, object>())
                    Add(place.Key, (string)Json.Field(place.Value, "address"), Json.Field(place.Value, "user") as string ?? "");
                Places.TryGetValue(Dimension.Key(Json.Field(settings, "default") as string), out fallback);
            }
            catch (Exception)
            {
                // No settings yet, or settings that cannot be read: start from the defaults.
            }
            fallback = fallback ?? Places.Values.FirstOrDefault() ?? Add(DefaultName, DefaultAddress, "");
            var excel = (Microsoft.Office.Interop.Excel.Application)ExcelDnaUtil.Application;
            Writeback.Watch(excel);
            Picker.Watch(excel);
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Choose element...")]
        public static void Choose()
        {
            var excel = (Microsoft.Office.Interop.Excel.Application)ExcelDnaUtil.Application;
            if (!Picker.Pick(excel.ActiveCell))
                MessageBox.Show("Select a cell whose formula is a SUBNM first.", Menu);
        }

        public void AutoClose()
        {
            foreach (var place in Places.Values)
                place.Connection.Dispose();
        }

        /// <summary>The connection a formula's cube or dimension asks for, and the name without
        /// its environment: "production:Sales" is Sales in production, "Sales" is Sales in the
        /// default environment. Null when there is no environment of that name.</summary>
        internal static Connection For(ref string name)
        {
            var colon = name.IndexOf(':');
            var environment = colon < 0 ? null : Dimension.Key(name.Substring(0, colon));
            name = name.Substring(colon + 1);
            lock (Gate)
            {
                var place = fallback;
                if (environment != null)
                    Places.TryGetValue(environment, out place);
                return place?.Connection;
            }
        }

        /// <summary>Adds an environment, or replaces the one of the same name.</summary>
        private static Place Add(string name, string address, string user, Connection connection = null)
        {
            var place = new Place { Name = name, User = user, Connection = connection ?? new Connection(address) };
            place.Connection.SignInNeeded += () =>
            {
                if (asking || place.Declined)
                    return;
                asking = true;
                ExcelAsyncUtil.QueueAsMacro(() => Connect(place));
            };
            // A changed model changes what the dimension functions answer, so every formula
            // asks again.
            place.Connection.ModelChanged += () => ExcelAsyncUtil.QueueAsMacro(Refresh);
            // Left unused, a transaction is rolled back by the server. What was entered in
            // it is gone, and the sheet goes back to showing what is stored.
            place.Connection.TransactionLost += () => ExcelAsyncUtil.QueueAsMacro(() =>
            {
                Say("Epiphany: the transaction was left too long and the server rolled it back.");
                Refresh();
            });
            lock (Gate)
            {
                if (Places.TryGetValue(Dimension.Key(name), out var replaced))
                    replaced.Connection.Dispose();
                Places[Dimension.Key(name)] = place;
            }
            return place;
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Refresh", ShortCut = "^R")]
        public static void Refresh()
        {
            Refreshes++;
            foreach (var place in Places.Values)
            {
                place.Declined = false;
                place.Connection.Refresh();
            }
            // Every formula is calculated again, whether or not Excel holds it to be out of date.
            ((Microsoft.Office.Interop.Excel.Application)ExcelDnaUtil.Application).CalculateFull();
        }

        /// <summary>What the status bar says when there is nothing else to say: that changes
        /// are being held in a transaction, or nothing, which gives the bar back to Excel.</summary>
        internal static object Standing => fallback.Connection.InTransaction
            ? "Epiphany: in a transaction. What you enter is yours alone until Commit; Roll back discards it."
            : (object)false;

        private static void Say(string message)
        {
            var excel = (Microsoft.Office.Interop.Excel.Application)ExcelDnaUtil.Application;
            excel.StatusBar = message ?? Standing;
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Begin transaction")]
        public static void BeginTransaction()
        {
            var connection = fallback.Connection;
            if (connection.InTransaction)
                MessageBox.Show("There is a transaction already. Commit it or roll it back first.", Menu);
            else if (!Task.Run(() => connection.Begin()).Result)
                MessageBox.Show("The server would not begin a transaction.", Menu);
            Say(null);
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Commit")]
        public static void CommitTransaction()
        {
            End(true);
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Roll back")]
        public static void RollbackTransaction()
        {
            End(false);
        }

        private static void End(bool commit)
        {
            var connection = fallback.Connection;
            if (!connection.InTransaction)
            {
                MessageBox.Show("There is no transaction. Begin one first.", Menu);
                return;
            }
            var problem = Task.Run(() => connection.End(commit)).Result;
            if (problem != null)
                MessageBox.Show("The transaction was not " + (commit ? "committed" : "rolled back") + ": " + problem, Menu);
            Say(null);
            Refresh();
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Connect...")]
        public static void Connect()
        {
            Connect(fallback);
        }

        /// <summary>Asks where an environment's server is and who is signing in, until the
        /// server accepts or the user gives up. The environment connected becomes the default.</summary>
        private static void Connect(Place place)
        {
            var values = new[] { place.Name, place.Connection.Address, place.User, "" };
            try
            {
                while (true)
                {
                    if (!Ask(values))
                    {
                        place.Declined = true;
                        return;
                    }
                    var problem = SignIn(values[0], values[1], values[2], values[3]);
                    if (problem == null)
                        break;
                    MessageBox.Show(problem, Menu, MessageBoxButtons.OK, MessageBoxIcon.Warning);
                }
            }
            finally
            {
                asking = false;
            }
            Save();
            Refresh();
        }

        /// <summary>Signs in and takes the environment into use. Null, or what went wrong.</summary>
        private static string SignIn(string name, string address, string user, string password)
        {
            if (Dimension.Key(name) == "" || name.Contains(":"))
                return "The environment needs a name, without a colon. Formulas use it as in \"" + DefaultName + ":Cube\".";
            Connection connection;
            try
            {
                connection = new Connection(address);
            }
            catch (UriFormatException)
            {
                return "'" + address + "' is not an address. It should look like " + DefaultAddress + ".";
            }
            if (!Task.Run(() => connection.SignIn(user, password)).Result)
            {
                connection.Dispose();
                return "The server at " + address + " could not be reached, or the name or password is wrong.";
            }
            fallback = Add(name, address, user, connection);
            return null;
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Disconnect")]
        public static void Disconnect()
        {
            foreach (var place in Places.Values.ToArray())
            {
                Task.Run(() => place.Connection.SignOut()).Wait();
                place.Declined = true;
            }
        }

        /// <summary>Remembers each environment's server and who signs in. Never a password.</summary>
        private static void Save()
        {
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(SettingsFile));
                var environments = Places.Values.ToDictionary(place => place.Name, place => (object)
                    new Dictionary<string, object> { ["address"] = place.Connection.Address, ["user"] = place.User });
                var settings = new Dictionary<string, object> { ["default"] = fallback.Name, ["environments"] = environments };
                File.WriteAllText(SettingsFile, Json.Write(settings));
            }
            catch (Exception)
            {
                // Not remembering is no reason to fail the connection that was just made.
            }
        }

        /// <summary>The window that asks for an environment, its server, a name and a password,
        /// starting from the values given and leaving in them what was typed.</summary>
        private static bool Ask(string[] values)
        {
            var labels = new[] { "Environment", "Server", "Name", "Password" };
            using (var form = new Form())
            {
                form.Text = "Connect to Epiphany";
                form.FormBorderStyle = FormBorderStyle.FixedDialog;
                form.StartPosition = FormStartPosition.CenterScreen;
                form.MinimizeBox = form.MaximizeBox = false;
                form.ClientSize = new System.Drawing.Size(360, 180);

                var boxes = new TextBox[labels.Length];
                for (var row = 0; row < labels.Length; row++)
                {
                    var top = 14 + row * 30;
                    form.Controls.Add(new Label { Text = labels[row], Left = 10, Top = top + 3, Width = 80 });
                    boxes[row] = new TextBox { Text = values[row], Left = 94, Top = top, Width = 256 };
                    form.Controls.Add(boxes[row]);
                }
                boxes[3].UseSystemPasswordChar = true;
                var ok = new Button { Text = "Connect", DialogResult = DialogResult.OK, Left = 184, Top = 142, Width = 80 };
                var cancel = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, Left = 270, Top = 142, Width = 80 };
                form.Controls.Add(ok);
                form.Controls.Add(cancel);
                form.AcceptButton = ok;
                form.CancelButton = cancel;
                // With a name already known, the password is what is left to type.
                form.ActiveControl = boxes[values[2] == "" ? 2 : 3];

                var accepted = form.ShowDialog() == DialogResult.OK;
                for (var row = 0; row < 3; row++)
                    values[row] = boxes[row].Text.Trim();
                values[3] = boxes[3].Text;
                return accepted;
            }
        }
    }
}
