using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Threading.Tasks;
using System.Windows.Forms;
using Range = Microsoft.Office.Interop.Excel.Range;

namespace Epiphany.Excel
{
    /// <summary>A double click on a cell whose formula is one SUBNM call lists the elements of
    /// its subset, and writes the one chosen into the formula.</summary>
    internal static class Picker
    {
        public static void Watch(Microsoft.Office.Interop.Excel.Application excel)
        {
            excel.SheetBeforeDoubleClick += (object sheet, Range cell, ref bool cancel) =>
            {
                // Once the list has been shown, the double click has done its work.
                cancel = Pick(cell);
            };
        }

        /// <summary>Lets the user choose the element of a SUBNM cell. False if the cell is not
        /// one, or its subset cannot be listed.</summary>
        public static bool Pick(Range cell)
        {
            try
            {
                var call = Formula.Call(cell.Formula as string ?? "");
                if (call == null || call[0] != "SUBNM" || call.Length < 3)
                    return false;
                // An argument may be a reference or a formula of its own; Excel works it out.
                Func<int, string> argument = index =>
                    index < call.Length && call[index] != "" ? Text(cell.Worksheet.Evaluate(call[index])) : "";
                var dimension = argument(1);
                var subset = argument(2);
                var alias = argument(4);
                var connection = AddIn.For(ref dimension);
                if (connection == null)
                    return false;
                var answer = Task.Run(() => connection.Ask(dimension, found => found.Members(subset, alias))).Result;
                if (!(answer is List<KeyValuePair<string, string>> members))
                    return false;
                var chosen = Choose(members, cell.Text as string);
                if (chosen != null)
                    cell.Formula = Formula.With(call, 3, Formula.Quote(chosen));
                return true;
            }
            catch (Exception)
            {
                // A cell that cannot be read is edited as any cell is.
                return false;
            }
        }

        private static string Text(object value)
        {
            if (value is Range range)
                value = range.Value2;
            return value is double number
                ? number.ToString("R", CultureInfo.InvariantCulture)
                : value as string ?? "";
        }

        /// <summary>The window: a box to narrow the list by typing, and the list. Gives the
        /// name of the element chosen, or null.</summary>
        private static string Choose(List<KeyValuePair<string, string>> members, string current)
        {
            using (var form = new Form())
            {
                form.Text = "Choose an element";
                form.FormBorderStyle = FormBorderStyle.FixedDialog;
                form.StartPosition = FormStartPosition.CenterScreen;
                form.MinimizeBox = form.MaximizeBox = false;
                form.ClientSize = new System.Drawing.Size(300, 360);

                var filter = new TextBox { Left = 10, Top = 10, Width = 280 };
                var list = new ListBox { Left = 10, Top = 40, Width = 280, Height = 270 };
                var ok = new Button { Text = "OK", DialogResult = DialogResult.OK, Left = 124, Top = 322, Width = 80 };
                var cancel = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, Left = 210, Top = 322, Width = 80 };
                form.Controls.AddRange(new Control[] { filter, list, ok, cancel });
                form.AcceptButton = ok;
                form.CancelButton = cancel;

                var listed = members;
                System.Action fill = () =>
                {
                    listed = members
                        .Where(member => member.Value.IndexOf(filter.Text.Trim(), StringComparison.OrdinalIgnoreCase) >= 0)
                        .ToList();
                    list.BeginUpdate();
                    list.Items.Clear();
                    list.Items.AddRange(listed.Select(member => (object)member.Value).ToArray());
                    list.EndUpdate();
                    // The element the cell shows now if it is listed, otherwise the first.
                    var shown = listed.FindIndex(member => member.Value == current);
                    list.SelectedIndex = listed.Count == 0 ? -1 : Math.Max(shown, 0);
                };
                filter.TextChanged += (sender, change) => fill();
                list.DoubleClick += (sender, click) => form.DialogResult = DialogResult.OK;
                fill();

                var accepted = form.ShowDialog() == DialogResult.OK && list.SelectedIndex >= 0;
                return accepted ? listed[list.SelectedIndex].Key : null;
            }
        }
    }
}
