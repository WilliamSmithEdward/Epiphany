using System.Globalization;
using ExcelDna.Integration;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// Worksheet functions (ADR-0022). Reads are asynchronous so the calc thread
/// never blocks on the network: <c>ExcelAsyncUtil.Run</c> runs the request on a
/// background thread, returns #N/A while it is in flight, and triggers a recalc
/// of the cell when the value arrives. Coordinates are given as "Dimension=Member"
/// tokens, which reads naturally in a formula.
/// </summary>
public static class Functions
{
    [ExcelFunction(
        Name = "EPIPHANY.READ",
        Description = "Read a cell value from an Epiphany cube. Give the cube name, then one \"Dimension=Member\" per dimension.",
        Category = "Epiphany")]
    public static object Read(
        [ExcelArgument(Name = "cube", Description = "The cube name")] string cube,
        [ExcelArgument(Name = "coords", Description = "One \"Dimension=Member\" per dimension")] params object[] coords)
    {
        if (AddIn.Client is null)
            return "#EPIPHANY: not connected - use the Epiphany ribbon to Connect";
        if (string.IsNullOrWhiteSpace(cube))
            return "#EPIPHANY: missing cube name";

        Dictionary<string, string> coord;
        try
        {
            coord = ParseCoord(coords);
        }
        catch (Exception e)
        {
            return "#EPIPHANY: " + e.Message;
        }

        // The async identity must be unique per distinct (cube, coordinate) call.
        var key = cube + "" + string.Join("", coord.OrderBy(kv => kv.Key).Select(kv => kv.Key + "=" + kv.Value));
        return ExcelAsyncUtil.Run("EPIPHANY.READ", new object[] { key }, () =>
        {
            try
            {
                var value = AddIn.Client!.ReadCellAsync(cube, coord).GetAwaiter().GetResult();
                return ToCell(value);
            }
            catch (Exception e)
            {
                return "#EPIPHANY: " + e.Message;
            }
        });
    }

    [ExcelFunction(
        Name = "EPIPHANY.STATUS",
        Description = "Show the current Epiphany connection.",
        Category = "Epiphany")]
    public static string Status()
        => AddIn.Client is null
            ? "Not connected"
            : $"Connected to {AddIn.Client.BaseUrl}" + (string.IsNullOrEmpty(AddIn.Client.Sandbox) ? "" : $" (sandbox: {AddIn.Client.Sandbox})");

    /// <summary>Parse "Dim=Member" tokens into a coordinate map.</summary>
    internal static Dictionary<string, string> ParseCoord(object[] tokens)
    {
        var coord = new Dictionary<string, string>();
        foreach (var raw in tokens)
        {
            if (raw is ExcelMissing or ExcelEmpty or null) continue;
            var token = Convert.ToString(raw, CultureInfo.InvariantCulture)?.Trim() ?? "";
            if (token.Length == 0) continue;
            var eq = token.IndexOf('=');
            if (eq <= 0 || eq == token.Length - 1)
                throw new FormatException($"\"{token}\" must be written Dimension=Member");
            var dim = token[..eq].Trim();
            var member = token[(eq + 1)..].Trim();
            if (dim.Length == 0 || member.Length == 0)
                throw new FormatException($"\"{token}\" must be written Dimension=Member");
            coord[dim] = member;
        }
        if (coord.Count == 0)
            throw new FormatException("give at least one Dimension=Member");
        return coord;
    }

    /// <summary>Map a server value (decimal string or null) to an Excel value.</summary>
    internal static object ToCell(string? value)
    {
        if (string.IsNullOrEmpty(value)) return ExcelEmpty.Value;
        return double.TryParse(value, NumberStyles.Any, CultureInfo.InvariantCulture, out var n)
            ? n
            : value;
    }
}
