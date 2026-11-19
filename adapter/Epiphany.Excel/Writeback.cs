using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Threading.Tasks;
using ExcelDna.Integration;
using Microsoft.Office.Interop.Excel;

namespace Epiphany.Excel
{
    /// <summary>Typing over a cell that holds a DBRW or DBR formula writes the typed value to
    /// the cube cell the formula reads, and puts the formula back. Two things are remembered
    /// to do it: where each such formula reads from, noted as it is calculated, and the
    /// formulas of the selected cells, noted before anything can be typed over them.</summary>
    internal static class Writeback
    {
        // Selections and changes of more cells than this are left alone.
        private const long Limit = 5000;

        private sealed class Source
        {
            public Connection Connection;
            public string Cube;
            public string[] At;
        }

        // The answer to a write that was made only at a second attempt, after the formulas
        // had been read again.
        private static readonly object Late = new object();
        private static readonly object Gate = new object();
        // By worksheet cell, as "[Book]Sheet!R1C1".
        private static readonly Dictionary<string, Source> Sources = new Dictionary<string, Source>();
        private static readonly Dictionary<string, string> Held = new Dictionary<string, string>();
        private static Application excel;

        public static void Watch(Application application)
        {
            excel = application;
            excel.SheetSelectionChange += Hold;
            excel.SheetActivate += sheet => Hold(sheet, excel.Selection as Range);
            excel.WorkbookActivate += book => Hold(excel.ActiveSheet, excel.Selection as Range);
            excel.SheetChange += Changed;
            Hold(excel.ActiveSheet, excel.Selection as Range);
        }

        /// <summary>Notes which cube cell the formula now being calculated reads.</summary>
        public static void Reads(Connection connection, string cube, string[] at)
        {
            if (!(XlCall.Excel(XlCall.xlfCaller) is ExcelReference caller))
                return;
            var key = Key((string)XlCall.Excel(XlCall.xlSheetNm, caller), caller.RowFirst + 1, caller.ColumnFirst + 1);
            lock (Gate)
                Sources[key] = new Source { Connection = connection, Cube = cube, At = at };
        }

        private static string Key(string sheet, int row, int column)
        {
            return sheet + "!R" + row + "C" + column;
        }

        private static string Key(Range cell)
        {
            var sheet = cell.Worksheet;
            return Key("[" + ((Workbook)sheet.Parent).Name + "]" + sheet.Name, cell.Row, cell.Column);
        }

        /// <summary>True for a formula that is one DBRW or DBR call and nothing else.</summary>
        internal static bool IsRead(string formula)
        {
            var open = formula.IndexOf('(');
            var name = open < 0 ? "" : formula.Substring(0, open).ToUpperInvariant();
            if (name != "=DBRW" && name != "=DBR")
                return false;
            var depth = 0;
            var quoted = false;
            for (var index = open; index < formula.Length; index++)
            {
                var character = formula[index];
                if (character == '"')
                    quoted = !quoted;
                else if (!quoted && character == '(')
                    depth++;
                else if (!quoted && character == ')' && --depth == 0)
                    return index == formula.Length - 1;
            }
            return false;
        }

        /// <summary>Remembers the read formulas of the cells now selected.</summary>
        private static void Hold(object sheet, Range selected)
        {
            try
            {
                Held.Clear();
                // False when no selected cell has a formula; then there is nothing to remember.
                if (!(sheet is Worksheet) || selected == null || Equals(selected.HasFormula, false)
                    || Convert.ToInt64(selected.CountLarge) > Limit)
                    return;
                foreach (Range cell in selected.Cells)
                    if (cell.Formula is string formula && IsRead(formula))
                        Held[Key(cell)] = formula;
            }
            catch (Exception)
            {
                // A selection that cannot be read is a selection nothing is written back from.
            }
        }

        private static void Changed(object sheet, Range changed)
        {
            try
            {
                if (!(sheet is Worksheet) || Convert.ToInt64(changed.CountLarge) > Limit)
                    return;
                var restored = new List<KeyValuePair<Range, string>>();
                var writes = new List<Task<object>>();
                foreach (Range cell in changed.Cells)
                {
                    var key = Key(cell);
                    var formula = cell.Formula as string ?? "";
                    Source source;
                    lock (Gate)
                        Sources.TryGetValue(key, out source);
                    if (formula.StartsWith("="))
                    {
                        // The formula itself was entered or edited.
                        Held.Remove(key);
                        if (IsRead(formula))
                            Held[key] = formula;
                    }
                    else if (source != null && Held.TryGetValue(key, out var held))
                    {
                        restored.Add(new KeyValuePair<Range, string>(cell, held));
                        var value = cell.Value2;
                        if (value is double || value is string)
                            writes.Add(Write(source, value));
                    }
                }
                if (restored.Count == 0)
                    return;
                // The writes were asked for first, so the formulas, calculated again as they
                // are put back, read what was written.
                excel.EnableEvents = false;
                try
                {
                    foreach (var cell in restored)
                        cell.Key.Formula = cell.Value;
                }
                finally
                {
                    excel.EnableEvents = true;
                }
                Report(writes);
            }
            catch (Exception)
            {
                // Whatever went wrong, the workbook is left as the user's edit made it.
            }
        }

        /// <summary>Writes a typed value. A number typed into a string cell is written as text.</summary>
        private static async Task<object> Write(Source source, object value)
        {
            var answer = await source.Connection.Write(source.Cube, source.At, value).ConfigureAwait(false);
            if (!Equals(answer, Connection.KeyError) || !(value is double number))
                return answer;
            answer = await source.Connection
                .Write(source.Cube, source.At, number.ToString("R", CultureInfo.InvariantCulture))
                .ConfigureAwait(false);
            // The formula was read again before this second write, so it shows the old text.
            return Equals(answer, Connection.KeyError) ? answer : Late;
        }

        /// <summary>Says in the status bar how many of the values could not be written, or
        /// clears what was said last time.</summary>
        private static async void Report(List<Task<object>> writes)
        {
            try
            {
                var answers = await Task.WhenAll(writes).ConfigureAwait(false);
                var refused = answers.Count(answer => Equals(answer, Connection.KeyError));
                var lost = answers.Count(answer => ReferenceEquals(answer, Connection.Unavailable));
                var message = refused + lost == 0 ? null
                    : "Epiphany: " + (refused + lost) + " of " + answers.Length + " values not written ("
                        + (refused > 0 ? "the cell is consolidated, calculated or of another type" : "no connection") + ")";
                var late = answers.Any(answer => ReferenceEquals(answer, Late));
                ExcelAsyncUtil.QueueAsMacro(() =>
                {
                    excel.StatusBar = message ?? (object)false;
                    if (late)
                        AddIn.Refresh();
                });
            }
            catch (Exception)
            {
                // The status bar is a courtesy.
            }
        }
    }
}
