using System.Collections.Generic;
using System.Linq;

namespace Epiphany.Excel
{
    /// <summary>Just enough reading of a worksheet formula to tell which function it calls and
    /// with which arguments, and to write it out again with one argument changed.</summary>
    internal static class Formula
    {
        /// <summary>The function, in upper case, and then the text of each argument, of a
        /// formula that is one function call and nothing else. Null for any other formula.</summary>
        public static string[] Call(string formula)
        {
            var open = formula.IndexOf('(');
            if (open < 2 || formula[0] != '=')
                return null;
            var call = new List<string> { formula.Substring(1, open - 1).Trim().ToUpperInvariant() };
            // Anything but a name before the bracket, an operator say, is more than a call.
            if (!call[0].All(character => char.IsLetterOrDigit(character) || character == '.' || character == '_'))
                return null;
            var depth = 0;
            // Text stands in double quotes, and the name of a sheet with spaces or signs in it
            // in single ones: 'Q1, plan (old)'!A1. Neither's commas and brackets are the call's.
            var quoted = false;
            var named = false;
            var start = open + 1;
            for (var index = open; index < formula.Length; index++)
            {
                var character = formula[index];
                if (character == '"' && !named)
                    quoted = !quoted;
                if (character == '\'' && !quoted)
                    named = !named;
                if (quoted || named)
                    continue;
                if (character == '(' || character == '{')
                    depth++;
                var closes = character == ')' || character == '}';
                if ((closes && --depth == 0) || (character == ',' && depth == 1))
                {
                    var argument = formula.Substring(start, index - start).Trim();
                    // A call with nothing between its brackets has no arguments, not one empty one.
                    if (argument != "" || call.Count > 1 || !closes)
                        call.Add(argument);
                    start = index + 1;
                }
                if (closes && depth == 0)
                    return index == formula.Length - 1 ? call.ToArray() : null;
            }
            return null;
        }

        /// <summary>The formula of a call, with one argument, counted from one, replaced.
        /// Arguments the call did not have up to that one are left empty.</summary>
        public static string With(string[] call, int argument, string text)
        {
            var changed = new List<string>(call);
            while (changed.Count <= argument)
                changed.Add("");
            changed[argument] = text;
            return "=" + changed[0] + "(" + string.Join(",", changed.GetRange(1, changed.Count - 1)) + ")";
        }

        /// <summary>Text as a formula writes it: in double quotes, with those inside doubled.</summary>
        public static string Quote(string text)
        {
            return "\"" + text.Replace("\"", "\"\"") + "\"";
        }
    }
}
