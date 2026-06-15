using System.Net.Http;
using System.Text;
using System.Text.Json;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// A thin REST client over the Epiphany API (ADR-0022). It holds the base URL and
/// bearer token in memory and carries the optional active sandbox header, exactly
/// like the web client. Numeric cell values stay decimal STRINGS end to end
/// (ADR-0008); conversion to a number happens only at the Excel boundary.
/// This client contains no model logic - the server is the single engine.
/// </summary>
public sealed class EpiphanyClient
{
    private static readonly HttpClient Shared = new() { Timeout = TimeSpan.FromSeconds(30) };
    private readonly HttpClient _http;

    public string BaseUrl { get; }
    public string? Token { get; set; }
    public string? Sandbox { get; set; }

    public EpiphanyClient(string baseUrl, string? token = null, HttpClient? http = null)
    {
        BaseUrl = baseUrl.TrimEnd('/');
        Token = token;
        _http = http ?? Shared;
    }

    private HttpRequestMessage Build(HttpMethod method, string path, object? body)
    {
        var req = new HttpRequestMessage(method, $"{BaseUrl}{path}");
        if (!string.IsNullOrEmpty(Token))
            req.Headers.TryAddWithoutValidation("Authorization", $"Bearer {Token}");
        if (!string.IsNullOrEmpty(Sandbox))
            req.Headers.TryAddWithoutValidation("X-Epiphany-Sandbox", Sandbox);
        if (body is not null)
            req.Content = new StringContent(JsonSerializer.Serialize(body), Encoding.UTF8, "application/json");
        return req;
    }

    private async Task<JsonDocument> SendAsync(HttpMethod method, string path, object? body)
    {
        using var req = Build(method, path, body);
        using var resp = await _http.SendAsync(req).ConfigureAwait(false);
        var text = await resp.Content.ReadAsStringAsync().ConfigureAwait(false);
        if (!resp.IsSuccessStatusCode)
            throw new EpiphanyException(ExtractMessage(text, (int)resp.StatusCode));
        return string.IsNullOrWhiteSpace(text) ? JsonDocument.Parse("null") : JsonDocument.Parse(text);
    }

    private static string ExtractMessage(string body, int status)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            if (doc.RootElement.TryGetProperty("error", out var err)
                && err.TryGetProperty("message", out var msg))
                return msg.GetString() ?? $"request failed ({status})";
        }
        catch
        {
            // fall through to the generic message
        }
        return $"request failed ({status})";
    }

    /// <summary>Health probe (no auth).</summary>
    public async Task<bool> PingAsync()
    {
        try
        {
            using var req = new HttpRequestMessage(HttpMethod.Get, $"{BaseUrl}/healthz");
            using var resp = await _http.SendAsync(req).ConfigureAwait(false);
            return resp.IsSuccessStatusCode;
        }
        catch
        {
            return false;
        }
    }

    /// <summary>The cube names visible to the signed-in user.</summary>
    public async Task<List<string>> ListCubesAsync()
    {
        using var doc = await SendAsync(HttpMethod.Get, "/api/v1/cubes", null).ConfigureAwait(false);
        var names = new List<string>();
        if (doc.RootElement.TryGetProperty("cubes", out var cubes))
            foreach (var c in cubes.EnumerateArray())
                if (c.TryGetProperty("name", out var n)) names.Add(n.GetString() ?? "");
        return names;
    }

    /// <summary>Read one cell value (decimal string, or null when empty/string-empty).</summary>
    public async Task<string?> ReadCellAsync(string cube, IDictionary<string, string> coord)
    {
        var results = await ReadCellsAsync(cube, new[] { coord }).ConfigureAwait(false);
        return results.Count > 0 ? results[0] : null;
    }

    /// <summary>Read many cells in one request; values line up with the coords.</summary>
    public async Task<List<string?>> ReadCellsAsync(string cube, IReadOnlyList<IDictionary<string, string>> coords)
    {
        var body = new { coords };
        using var doc = await SendAsync(HttpMethod.Post,
            $"/api/v1/cubes/{Uri.EscapeDataString(cube)}/cells/read", body).ConfigureAwait(false);
        var values = new List<string?>();
        if (doc.RootElement.TryGetProperty("cells", out var cells))
            foreach (var cell in cells.EnumerateArray())
                values.Add(cell.TryGetProperty("value", out var v) && v.ValueKind != JsonValueKind.Null
                    ? v.GetString()
                    : null);
        return values;
    }

    /// <summary>Apply a transactional batch of leaf writes; returns the count applied.</summary>
    public async Task<int> BatchWriteAsync(string cube, IReadOnlyList<CellWrite> writes)
    {
        var body = new { writes = writes.Select(w => new { coord = w.Coord, value = w.Value }) };
        using var doc = await SendAsync(HttpMethod.Post,
            $"/api/v1/cubes/{Uri.EscapeDataString(cube)}/cells/batch", body).ConfigureAwait(false);
        return doc.RootElement.TryGetProperty("applied", out var a) ? a.GetInt32() : writes.Count;
    }
}

/// <summary>One coordinate plus the value to write (a decimal string or text).</summary>
public sealed record CellWrite(Dictionary<string, string> Coord, string Value);

/// <summary>A server-or-transport error carrying a client-safe message.</summary>
public sealed class EpiphanyException(string message) : Exception(message);
