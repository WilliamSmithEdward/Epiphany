using System.Collections.Generic;
using System.Globalization;
using System.Text.RegularExpressions;

namespace Epiphany.Excel
{
    /// <summary>
    /// What may be typed over a consolidation to spread a value over the leaves beneath it, as
    /// TM1's clients have it, and as the GUI's grid takes it: a code and then the value,
    /// "P1200" say. P spreads in proportion to what the leaves hold, S (or LS) in equal shares,
    /// R (or LR) gives each leaf the value itself, and C, alone, clears them. After P or S, a
    /// "+" or "~" before the value adds it to what the cell holds, or takes it away.
    /// </summary>
    public sealed class SpreadCode
    {
        private static readonly Regex Typed = new Regex(
            @"^\s*(LS|LR|P|S|R|C)\s*([+~])?\s*(.*?)\s*$",
            RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);

        private static readonly Dictionary<string, string> Methods = new Dictionary<string, string>
        {
            ["P"] = "proportional", ["S"] = "equal", ["LS"] = "equal",
            ["R"] = "repeat", ["LR"] = "repeat", ["C"] = "repeat",
        };

        /// <summary>The server's name for the way the value is shared out.</summary>
        public string Method { get; private set; }

        /// <summary>The value typed: the cell's new value, or what changes it.</summary>
        public double Value { get; private set; }

        /// <summary>1 where the value is added to what the cell holds, -1 where it is taken
        /// from it, and 0 where it is the cell's new value.</summary>
        public int Change { get; private set; }

        /// <summary>The spread a text says, or null where it says none.</summary>
        public static SpreadCode Parse(object typed)
        {
            var match = Typed.Match(typed as string ?? "");
            if (!match.Success)
                return null;
            var code = match.Groups[1].Value.ToUpperInvariant();
            var change = match.Groups[2].Value == "+" ? 1 : match.Groups[2].Value == "~" ? -1 : 0;
            var rest = match.Groups[3].Value;
            if (code == "C")
                return change == 0 && rest.Length == 0 ? new SpreadCode { Method = "repeat", Value = 0 } : null;
            var method = Methods[code];
            if (!double.TryParse(rest, NumberStyles.Float, CultureInfo.InvariantCulture, out var value)
                || (change != 0 && method == "repeat"))
                return null;
            return new SpreadCode { Method = method, Value = value, Change = change };
        }

        /// <summary>What the cell comes to once spread, from what it holds before.</summary>
        public double ComesTo(double before)
        {
            return Change == 0 ? Value : before + Change * Value;
        }
    }
}
