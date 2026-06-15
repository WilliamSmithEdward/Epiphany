using System.Security.Cryptography;
using System.Text;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// Persists the connection across sessions (ADR-0022): the base URL in clear and
/// the bearer token DPAPI-encrypted to the current Windows user. Files live under
/// %LOCALAPPDATA%\Epiphany; the token is never written to a workbook and never
/// logged. A failure to read (e.g. a different user, or corruption) is treated as
/// "not connected" rather than an error.
/// </summary>
public static class TokenStore
{
    private static string Dir =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Epiphany");

    private static string TokenFile => Path.Combine(Dir, "token.bin");
    private static string BaseFile => Path.Combine(Dir, "base.txt");

    public static void Save(string baseUrl, string token)
    {
        Directory.CreateDirectory(Dir);
        File.WriteAllText(BaseFile, baseUrl.TrimEnd('/'));
        var cipher = ProtectedData.Protect(
            Encoding.UTF8.GetBytes(token), optionalEntropy: null, DataProtectionScope.CurrentUser);
        File.WriteAllBytes(TokenFile, cipher);
    }

    public static (string BaseUrl, string Token)? Load()
    {
        try
        {
            if (!File.Exists(TokenFile) || !File.Exists(BaseFile)) return null;
            var baseUrl = File.ReadAllText(BaseFile).Trim();
            var plain = ProtectedData.Unprotect(
                File.ReadAllBytes(TokenFile), optionalEntropy: null, DataProtectionScope.CurrentUser);
            return (baseUrl, Encoding.UTF8.GetString(plain));
        }
        catch
        {
            return null;
        }
    }

    /// <summary>The remembered base URL, even when no token is stored.</summary>
    public static string? LastBaseUrl()
    {
        try
        {
            return File.Exists(BaseFile) ? File.ReadAllText(BaseFile).Trim() : null;
        }
        catch
        {
            return null;
        }
    }

    public static void Clear()
    {
        try
        {
            if (File.Exists(TokenFile)) File.Delete(TokenFile);
        }
        catch
        {
            // best effort; the in-memory client is cleared regardless
        }
    }
}
