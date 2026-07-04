using System.Globalization;
using ExcelDna.Integration;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// Worksheet functions (ADR-0022). Reads are asynchronous so the calc thread
/// never blocks on the network: <c>ExcelAsyncUtil.Observe</c> hands the request to
/// an <see cref="IExcelObservable"/> that ExcelDna hosts on its RTD topic, returns
/// #N/A while it is in flight, and pushes the value to the cell when it arrives -
/// without parking a pool thread per cell. The individual cell reads of one recalc
/// are gathered by <see cref="ReadCoalescer"/> into a single <c>cells/read</c> POST
/// per (server, sandbox, cube), so a workbook of many reads costs one round trip,
/// not one per cell. Coordinates are given as "Dimension=Member" tokens, which
/// reads naturally in a formula.
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
        // Capture the client once: it is a static mutated from the UI thread
        // (Sign out sets it null, Connect replaces it), so the background lambda
        // must use this captured reference - re-reading AddIn.Client there could
        // NullReferenceException if the user signs out while a read is in flight.
        var client = AddIn.Client;
        if (client is null)
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

        // The async identity must be unique per distinct (server, sandbox,
        // cube, coordinate) call: ExcelAsyncUtil.Observe dedupes topics and
        // delivers by these parameters. Include BaseUrl and Sandbox so a read in flight when
        // the user switches sandbox/server is not matched by identity and
        // delivered into the wrong context. Separate every part with  (a
        // control char no legal name contains) so distinct requests cannot
        // collide into one key.
        var coordKey = string.Join("", coord.OrderBy(kv => kv.Key, StringComparer.Ordinal)
            .Select(kv => kv.Key + "" + kv.Value));
        var identity = new object[] { client.BaseUrl, client.Sandbox ?? "", cube, coordKey };
        // Observe (not Run): the coalescer batches this cell's read with the other
        // reads of the same recalc into one POST and pushes the value back through
        // the observable. ExcelDna hosts the observable on an RTD topic, so no pool
        // thread is parked while the batch is in flight.
        return ExcelAsyncUtil.Observe("EPIPHANY.READ", identity,
            () => ReadCoalescer.Instance.Subscribe(client, cube, coord));
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
