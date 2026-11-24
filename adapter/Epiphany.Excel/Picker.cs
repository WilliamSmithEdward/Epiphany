using System;
using System.Collections.Generic;
using System.Drawing;
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
                var answer = AddIn.Awaiting("reading the elements of " + dimension,
                    () => connection.Ask(dimension, found => found.Members(subset, alias)));
                if (!(answer is List<KeyValuePair<string, string>> members))
                    return false;
                string chosen;
                using (var window = new ChoiceWindow(members, cell.Text as string))
                    chosen = window.ShowDialog() == DialogResult.OK ? window.Chosen : null;
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
    }

    /// <summary>The window that lists the elements to choose from: a box that narrows the list
    /// to those whose shown name holds what is typed, the list, and how many it shows. The list
    /// asks only for the rows in sight, so that a subset of a million elements is listed and
    /// narrowed at each key at once, where a list holding every row was filled afresh each
    /// time. Up, Down, Page Up and Page Down in the box move the choice.</summary>
    internal sealed class ChoiceWindow : Form
    {
        // How far Page Up and Page Down move, and how many rows the window opens to at most.
        private const int Page = 10;
        private const int MostRows = 20;
        private const int Gap = 10;

        private readonly List<KeyValuePair<string, string>> members;
        private readonly string current;
        private readonly TextBox filter = new TextBox();
        private readonly ListView list = new ListView
        {
            View = View.Details,
            HeaderStyle = ColumnHeaderStyle.None,
            FullRowSelect = true,
            MultiSelect = false,
            HideSelection = false,
            VirtualMode = true,
        };
        private readonly Label count = new Label { AutoSize = true };
        private List<KeyValuePair<string, string>> listed;
        // The row chosen, or -1. Kept here, for a virtual list holds no choice until it is shown.
        private int selected = -1;
        // How many rows the list opens to show.
        private readonly int rows;

        /// <summary>The elements by name, each with what it is shown as; and what the cell shows
        /// now, chosen to begin with where it is listed.</summary>
        public ChoiceWindow(List<KeyValuePair<string, string>> members, string current)
        {
            this.members = members;
            this.current = current;
            listed = members;
            Text = "Choose an element";
            StartPosition = FormStartPosition.CenterScreen;
            MinimizeBox = MaximizeBox = false;
            ShowInTaskbar = false;
            ShowIcon = false;

            var ok = new Button { Text = "OK", DialogResult = DialogResult.OK, Width = 80 };
            var cancel = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, Width = 80 };
            AcceptButton = ok;
            CancelButton = cancel;
            list.Columns.Add("");

            // As wide as the widest name, up to a part of the screen; drawn larger, it shows more.
            rows = Math.Max(5, Math.Min(MostRows, members.Count));
            var width = Math.Max(300, Math.Min(Screen.PrimaryScreen.WorkingArea.Width * 4 / 5,
                Widest() + 2 * Gap + SystemInformation.VerticalScrollBarWidth + 12));
            filter.SetBounds(Gap, Gap, width - 2 * Gap, filter.Height);
            list.SetBounds(Gap, filter.Bottom + 6, width - 2 * Gap, 100);
            cancel.SetBounds(width - Gap - cancel.Width, list.Bottom + 8, cancel.Width, cancel.Height);
            ok.SetBounds(cancel.Left - 6 - ok.Width, cancel.Top, ok.Width, ok.Height);
            count.Location = new Point(Gap, cancel.Top + (cancel.Height - count.PreferredHeight) / 2);
            ClientSize = new Size(width, cancel.Bottom + Gap);
            filter.Anchor = AnchorStyles.Top | AnchorStyles.Left | AnchorStyles.Right;
            list.Anchor = AnchorStyles.Top | AnchorStyles.Bottom | AnchorStyles.Left | AnchorStyles.Right;
            ok.Anchor = cancel.Anchor = AnchorStyles.Bottom | AnchorStyles.Right;
            count.Anchor = AnchorStyles.Bottom | AnchorStyles.Left;
            Controls.AddRange(new Control[] { filter, list, count, ok, cancel });
            // A row's height is known once the list has its window; until then, near enough.
            Rows(TextRenderer.MeasureText("Ag", list.Font).Height + 5, 2);
            Fit();

            list.RetrieveVirtualItem += (sender, wanted) =>
                wanted.Item = new ListViewItem(wanted.ItemIndex < listed.Count ? listed[wanted.ItemIndex].Value : "");
            list.SelectedIndexChanged += (sender, change) =>
                selected = list.SelectedIndices.Count > 0 ? list.SelectedIndices[0] : -1;
            list.DoubleClick += (sender, click) => DialogResult = DialogResult.OK;
            list.Resize += (sender, change) => Fit();
            filter.TextChanged += (sender, change) => Fill();
            filter.KeyDown += (sender, key) => key.Handled = Press(key.KeyCode);
            Fill();
        }

        /// <summary>The name of the element chosen, or null.</summary>
        public string Chosen => selected < 0 ? null : listed[selected].Key;

        /// <summary>What is typed in the box.</summary>
        internal string Typed
        {
            get => filter.Text;
            set => filter.Text = value;
        }

        /// <summary>How many elements are listed.</summary>
        internal int Listed => listed.Count;

        /// <summary>What the window says of how many there are.</summary>
        internal string Counted => count.Text;

        /// <summary>Moves the choice as a key typed in the box asks; whether it was one that does.</summary>
        internal bool Press(Keys key)
        {
            var step = key == Keys.Down ? 1 : key == Keys.Up ? -1 : key == Keys.PageDown ? Page : key == Keys.PageUp ? -Page : 0;
            if (step == 0)
                return false;
            if (listed.Count > 0)
                Choose(Math.Max(0, Math.Min(listed.Count - 1, selected + step)));
            return true;
        }

        protected override void OnLoad(EventArgs e)
        {
            base.OnLoad(e);
            if (listed.Count > 0)
            {
                var first = list.GetItemRect(0);
                Rows(first.Height, first.Top);
            }
            Choose(selected);
        }

        // Makes the list as tall as the rows it opens to show, the first of them so far down,
        // up to half the screen; and lets the window be made no smaller than three rows.
        private void Rows(int row, int top)
        {
            // The list's edge, above and below; not what its client area leaves, which a scroll
            // bar about to go takes from too.
            var frame = 2 * SystemInformation.Border3DSize.Height + top;
            MinimumSize = Size.Empty;
            Height += Math.Min(Screen.PrimaryScreen.WorkingArea.Height / 2, rows * row + frame) - list.Height;
            MinimumSize = new Size(Width - ClientSize.Width + 260, Height - list.Height + 3 * row + frame);
        }

        // The elements whose shown name holds what is typed, and the one the cell shows if it is
        // among them, otherwise the first.
        private void Fill()
        {
            var typed = filter.Text.Trim();
            listed = typed == ""
                ? members
                : members.Where(member => member.Value.IndexOf(typed, StringComparison.OrdinalIgnoreCase) >= 0).ToList();
            list.SelectedIndices.Clear();
            list.VirtualListSize = listed.Count;
            list.Invalidate();
            count.Text = listed.Count == members.Count
                ? Number(members.Count) + (members.Count == 1 ? " element" : " elements")
                : Number(listed.Count) + " of " + Number(members.Count);
            Choose(listed.Count == 0 ? -1 : Math.Max(listed.FindIndex(member => member.Value == current), 0));
        }

        private void Choose(int row)
        {
            selected = row;
            if (!list.IsHandleCreated)
                return;
            list.SelectedIndices.Clear();
            if (row < 0)
                return;
            list.SelectedIndices.Add(row);
            list.EnsureVisible(row);
        }

        private void Fit()
        {
            list.Columns[0].Width = list.ClientSize.Width;
        }

        private static string Number(int number)
        {
            return number.ToString("N0", CultureInfo.CurrentCulture);
        }

        // The width of the widest shown name, measured on the longest few by their characters:
        // a million measured took seconds.
        private int Widest()
        {
            const int Few = 50;
            var longest = new List<string>();
            var shortest = 0;
            foreach (var member in members)
            {
                var name = member.Value ?? "";
                if (longest.Count == Few && name.Length <= shortest)
                    continue;
                if (longest.Count == Few)
                    longest.Remove(longest.First(kept => kept.Length == shortest));
                longest.Add(name);
                shortest = longest.Min(kept => kept.Length);
            }
            return longest.Select(name => TextRenderer.MeasureText(name, list.Font).Width).DefaultIfEmpty(0).Max();
        }
    }
}
