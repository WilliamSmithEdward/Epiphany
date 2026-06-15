using System.Text.Json;
using System.Windows.Forms;
using Microsoft.Web.WebView2.Core;
using Microsoft.Web.WebView2.WinForms;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// The connect/login screen (ADR-0022). The user types the server base URL and
/// the embedded WebView2 loads the server's own React login. On a successful
/// login the page posts the session token back via
/// <c>window.chrome.webview.postMessage</c>; this host validates that the message
/// originated from the configured server before accepting the token, stores it
/// DPAPI-encrypted, and wires up the shared client. Reusing the server login
/// keeps all auth logic on the server (one login, not two).
/// </summary>
public sealed class ConfiguratorForm : Form
{
    private readonly TextBox _urlBox = new();
    private readonly Button _goButton = new();
    private readonly Label _statusLabel = new();
    private readonly WebView2 _web = new();
    private string _baseUrl = "";

    public ConfiguratorForm(string? initialUrl)
    {
        Text = "Connect to Epiphany";
        Width = 920;
        Height = 720;
        StartPosition = FormStartPosition.CenterScreen;

        var bar = new Panel { Dock = DockStyle.Top, Height = 44, Padding = new Padding(8) };
        _urlBox.Text = initialUrl ?? "https://localhost:8443";
        _urlBox.Dock = DockStyle.Fill;
        _goButton.Text = "Connect";
        _goButton.Dock = DockStyle.Right;
        _goButton.Width = 100;
        _goButton.Click += (_, _) => Navigate();
        bar.Controls.Add(_urlBox);
        bar.Controls.Add(_goButton);

        _statusLabel.Dock = DockStyle.Bottom;
        _statusLabel.Height = 24;
        _statusLabel.Padding = new Padding(8, 4, 8, 4);
        _statusLabel.Text = "Enter your Epiphany server address and sign in.";

        _web.Dock = DockStyle.Fill;

        Controls.Add(_web);
        Controls.Add(_statusLabel);
        Controls.Add(bar);

        Load += async (_, _) => await InitWebViewAsync();
    }

    private async Task InitWebViewAsync()
    {
        try
        {
            await _web.EnsureCoreWebView2Async();
            _web.CoreWebView2.WebMessageReceived += OnWebMessage;
        }
        catch (Exception e)
        {
            _statusLabel.Text = "Could not start the embedded browser (WebView2 runtime missing?): " + e.Message;
        }
    }

    private void Navigate()
    {
        var url = _urlBox.Text.Trim().TrimEnd('/');
        if (url.Length == 0) return;
        _baseUrl = url;
        try
        {
            _web.CoreWebView2?.Navigate(url + "/");
            _statusLabel.Text = "Sign in to continue. The token never leaves this machine.";
        }
        catch (Exception e)
        {
            _statusLabel.Text = "Navigation failed: " + e.Message;
        }
    }

    /// <summary>
    /// Accept a token only when the message came from the configured server's
    /// origin (origin-validated postMessage, ADR-0022).
    /// </summary>
    private void OnWebMessage(object? sender, CoreWebView2WebMessageReceivedEventArgs e)
    {
        if (!OriginMatches(e.Source, _baseUrl))
        {
            _statusLabel.Text = "Ignored a message from an unexpected origin.";
            return;
        }

        string? token = null;
        try
        {
            // The page may post a JSON envelope { type: "epiphany-auth", token }
            // or a bare token string; accept either.
            var json = e.WebMessageAsJson;
            using var doc = JsonDocument.Parse(json);
            if (doc.RootElement.ValueKind == JsonValueKind.String)
                token = doc.RootElement.GetString();
            else if (doc.RootElement.TryGetProperty("type", out var t)
                     && t.GetString() == "epiphany-auth"
                     && doc.RootElement.TryGetProperty("token", out var tok))
                token = tok.GetString();
        }
        catch
        {
            // not a token message; ignore
        }

        if (string.IsNullOrEmpty(token)) return;

        TokenStore.Save(_baseUrl, token);
        AddIn.Client = new EpiphanyClient(_baseUrl, token);
        _statusLabel.Text = "Connected.";
        DialogResult = DialogResult.OK;
        Close();
    }

    /// <summary>True when the message source URL has the same scheme/host/port as the base.</summary>
    internal static bool OriginMatches(string source, string baseUrl)
    {
        return Uri.TryCreate(source, UriKind.Absolute, out var a)
            && Uri.TryCreate(baseUrl, UriKind.Absolute, out var b)
            && string.Equals(a.Scheme, b.Scheme, StringComparison.OrdinalIgnoreCase)
            && string.Equals(a.Host, b.Host, StringComparison.OrdinalIgnoreCase)
            && a.Port == b.Port;
    }
}
