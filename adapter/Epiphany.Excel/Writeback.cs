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
    /// the cube cell the formula reads, and puts the formula back. One thing is remembered to
    /// do it: the formulas of the selected cells, noted before anything can be typed over
    /// them. Which cube cell a formula reads is worked out from the formula when it is typed
    /// over, and not kept by where the formula stood: rows are inserted and ranges sorted,
    /// and a note kept by address would then be a note about another formula.</summary>
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

        // The read formulas of the selected cells, by worksheet cell as "[Book]Sheet!R1C1".
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

        /// <summary>The cube cell a read formula reads, each argument worked out by Excel as it
        /// stands on the formula's sheet. Null if an argument has no value just now or names
        /// an environment that is not known.</summary>
        private static Source Read(Worksheet sheet, string formula)
        {
            var arguments = new List<string>();
            foreach (var argument in Formula.Call(formula).Skip(1))
            {
                var value = sheet.Evaluate(argument);
                if (value is Range range)
                    value = range.Value2;
                // A number is a name too, a year say; anything else is an error or nothing.
                var text = value is double number
                    ? number.ToString("R", CultureInfo.InvariantCulture)
                    : value as string;
                if (text == null)
                    return null;
                arguments.Add(text);
            }
            if (arguments.Count == 0)
                return null;
            var cube = arguments[0];
            var connection = AddIn.For(ref cube);
            return connection == null
                ? null
                : new Source { Connection = connection, Cube = cube, At = arguments.Skip(1).ToArray() };
        }

        private static string Key(Range cell)
        {
            var sheet = cell.Worksheet;
            return "[" + ((Workbook)sheet.Parent).Name + "]" + sheet.Name + "!R" + cell.Row + "C" + cell.Column;
        }

        /// <summary>True for a formula that is one DBRW or DBR call and nothing else.</summary>
        internal static bool IsRead(string formula)
        {
            var function = Formula.Call(formula)?[0];
            return function == "DBRW" || function == "DBR";
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
                    if (formula.StartsWith("="))
                    {
                        // The formula itself was entered or edited.
                        Held.Remove(key);
                        if (IsRead(formula))
                            Held[key] = formula;
                    }
                    else if (Held.TryGetValue(key, out var held))
                    {
                        restored.Add(new KeyValuePair<Range, string>(cell, held));
                        var value = cell.Value2;
                        if (!(value is double) && !(value is string))
                            continue;
                        // A formula whose cube cell cannot be told is put back with nothing
                        // written, and counted among the values that were not.
                        var source = Read(cell.Worksheet, held);
                        writes.Add(source == null ? Task.FromResult<object>(Connection.KeyError) : Write(source, value));
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

        /// <summary>Writes a typed value. A number typed into a string cell is written as text,
        /// and a spreading code typed over a consolidation spreads its number.</summary>
        private static async Task<object> Write(Source source, object value)
        {
            // What was typed can be meant in more than one way, and only the server knows which
            // fits the cell. Each is tried in turn until one is taken: a spread, where it has
            // the form of one; the value as typed; and then the number of a spread written to
            // a leaf, or a number written to a string cell as text.
            var spread = SpreadCode.Parse(value);
            double? comesTo = null;
            if (spread != null && spread.Change != 0)
            {
                // Added to or taken from what the cell holds, which is asked for first.
                var before = await source.Connection.Read(source.Cube, source.At).ConfigureAwait(false);
                if (before is double held)
                    comesTo = spread.ComesTo(held);
            }
            else if (spread != null)
                comesTo = spread.Value;
            var answer = (object)Connection.KeyError;
            if (comesTo.HasValue)
                answer = await source.Connection
                    .Spread(source.Cube, source.At, comesTo.Value, spread.Method)
                    .ConfigureAwait(false);
            if (Equals(answer, Connection.KeyError))
                answer = await source.Connection.Write(source.Cube, source.At, value).ConfigureAwait(false);
            if (!Equals(answer, Connection.KeyError))
                return answer;
            object other = null;
            if (comesTo.HasValue)
                other = comesTo.Value;
            else if (value is double number)
                other = number.ToString("R", CultureInfo.InvariantCulture);
            return other == null
                ? answer
                : await source.Connection.Write(source.Cube, source.At, other).ConfigureAwait(false);
        }

        /// <summary>Says in the status bar how many of the values could not be written, or
        /// clears what was said last time, and has every formula ask the server again, so
        /// that those which read what was written show it.</summary>
        private static async void Report(List<Task<object>> writes)
        {
            try
            {
                var answers = await Task.WhenAll(writes).ConfigureAwait(false);
                var refused = answers.Count(answer => Equals(answer, Connection.KeyError));
                var lost = answers.Count(answer => ReferenceEquals(answer, Connection.Unavailable));
                var message = refused + lost == 0 ? null
                    : "Epiphany: " + (refused + lost) + " of " + answers.Length + " values not written ("
                        + (refused > 0 ? "the cell is consolidated, calculated, of another type or not to be found" : "no connection") + ")";
                ExcelAsyncUtil.QueueAsMacro(() =>
                {
                    excel.StatusBar = message ?? AddIn.Standing;
                    if (refused + lost < answers.Length)
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
