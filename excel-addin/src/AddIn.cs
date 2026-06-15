using ExcelDna.Integration;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// Add-in lifecycle (ADR-0022). On open it restores a saved connection (base URL
/// plus DPAPI-decrypted token) so an already-connected user is ready immediately;
/// the ribbon's Connect button (re)establishes one. The single shared client is
/// the only place the token lives in memory.
/// </summary>
public sealed class AddIn : IExcelAddIn
{
    /// <summary>The active connection, or null when signed out.</summary>
    public static EpiphanyClient? Client { get; set; }

    public void AutoOpen()
    {
        var saved = TokenStore.Load();
        if (saved is { } s)
            Client = new EpiphanyClient(s.BaseUrl, s.Token);
    }

    public void AutoClose()
    {
        Client = null;
    }
}
