using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Threading.Tasks;
using System.Windows.Forms;
using ExcelDna.Integration;

namespace Epiphany.Excel
{
    /// <summary>The add-in itself: the one connection the worksheet functions use, and the
    /// commands of the Epiphany menu.</summary>
    public sealed class AddIn : IExcelAddIn
    {
        private const string DefaultAddress = "http://127.0.0.1:8080";
        private const string Menu = "Epiphany";

        private static readonly string SettingsFile = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "Epiphany", "adapter.json");

        internal static Connection Connection { get; private set; }

        private static string user = "";
        // The sign-in window is open or on its way.
        private static bool asking;
        // The user closed the sign-in window without signing in; do not open it again unasked.
        private static bool declined;

        public void AutoOpen()
        {
            var address = DefaultAddress;
            try
            {
                var settings = Json.Read(File.ReadAllText(SettingsFile));
                address = Json.Field(settings, "address") as string ?? address;
                user = Json.Field(settings, "user") as string ?? "";
            }
            catch (Exception)
            {
                // No settings yet, or settings that cannot be read: start from the defaults.
            }
            Use(address);
        }

        public void AutoClose()
        {
            Connection?.Dispose();
        }

        private static void Use(string address)
        {
            Connection?.Dispose();
            Connection = new Connection(address);
            Connection.SignInNeeded += () =>
            {
                if (asking || declined)
                    return;
                asking = true;
                ExcelAsyncUtil.QueueAsMacro(Connect);
            };
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Refresh", ShortCut = "^R")]
        public static void Refresh()
        {
            declined = false;
            Connection.Refresh();
            // Every formula is calculated again, whether or not Excel holds it to be out of date.
            var excel = ExcelDnaUtil.Application;
            excel.GetType().InvokeMember("CalculateFull", BindingFlags.InvokeMethod, null, excel, null);
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Connect...")]
        public static void Connect()
        {
            try
            {
                while (true)
                {
                    string address, name, password;
                    if (!Ask(out address, out name, out password))
                    {
                        declined = true;
                        return;
                    }
                    string problem = null;
                    try
                    {
                        Use(address);
                        var connection = Connection;
                        if (!Task.Run(() => connection.SignIn(name, password)).Result)
                            problem = "The server at " + address + " could not be reached, or the name or password is wrong.";
                    }
                    catch (UriFormatException)
                    {
                        problem = "'" + address + "' is not an address. It should look like " + DefaultAddress + ".";
                    }
                    if (problem == null)
                    {
                        user = name;
                        Save(address);
                        break;
                    }
                    MessageBox.Show(problem, Menu, MessageBoxButtons.OK, MessageBoxIcon.Warning);
                }
            }
            finally
            {
                asking = false;
            }
            Refresh();
        }

        [ExcelCommand(MenuName = Menu, MenuText = "Disconnect")]
        public static void Disconnect()
        {
            var connection = Connection;
            Task.Run(() => connection.SignOut()).Wait();
            declined = true;
        }

        /// <summary>Remembers where the server is and who signs in. Never the password.</summary>
        private static void Save(string address)
        {
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(SettingsFile));
                var settings = new Dictionary<string, object> { ["address"] = address, ["user"] = user };
                File.WriteAllText(SettingsFile, Json.Write(settings));
            }
            catch (Exception)
            {
                // Not remembering is no reason to fail the connection that was just made.
            }
        }

        /// <summary>The window that asks where the server is and who is signing in.</summary>
        private static bool Ask(out string address, out string name, out string password)
        {
            using (var form = new Form())
            {
                form.Text = "Connect to Epiphany";
                form.FormBorderStyle = FormBorderStyle.FixedDialog;
                form.StartPosition = FormStartPosition.CenterScreen;
                form.MinimizeBox = form.MaximizeBox = false;
                form.ClientSize = new System.Drawing.Size(360, 150);

                var fields = new[]
                {
                    Field(form, "Server", Connection.Address, 0),
                    Field(form, "Name", user, 1),
                    Field(form, "Password", "", 2),
                };
                fields[2].UseSystemPasswordChar = true;
                var ok = new Button { Text = "Connect", DialogResult = DialogResult.OK, Left = 184, Top = 112, Width = 80 };
                var cancel = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, Left = 270, Top = 112, Width = 80 };
                form.Controls.Add(ok);
                form.Controls.Add(cancel);
                form.AcceptButton = ok;
                form.CancelButton = cancel;
                // With a name already known, the password is what is left to type.
                form.ActiveControl = fields[user == "" ? 1 : 2];

                var accepted = form.ShowDialog() == DialogResult.OK;
                address = fields[0].Text.Trim();
                name = fields[1].Text.Trim();
                password = fields[2].Text;
                return accepted;
            }
        }

        private static TextBox Field(Form form, string label, string text, int row)
        {
            var top = 14 + row * 30;
            form.Controls.Add(new Label { Text = label, Left = 10, Top = top + 3, Width = 70 });
            var box = new TextBox { Text = text, Left = 84, Top = top, Width = 266 };
            form.Controls.Add(box);
            return box;
        }
    }
}
