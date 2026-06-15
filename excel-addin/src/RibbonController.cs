using System.Runtime.InteropServices;
using System.Windows.Forms;
using ExcelDna.Integration.CustomUI;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// The Epiphany ribbon tab (ADR-0022): Connect / Sign out, Commit selection, a
/// what-if sandbox box, and a Status readout. Everything is spelled out; errors
/// surface as plain dialogs, never raw stack traces.
/// </summary>
[ComVisible(true)]
public sealed class RibbonController : ExcelRibbon
{
    private IRibbonUI? _ribbon;

    public override string GetCustomUI(string ribbonId) => """
        <customUI xmlns="http://schemas.microsoft.com/office/2009/07/customui" onLoad="OnLoad">
          <ribbon>
            <tabs>
              <tab id="epiphanyTab" label="Epiphany">
                <group id="connGroup" label="Connection">
                  <button id="connect" label="Connect" size="large" imageMso="ServerConnection"
                          onAction="OnConnect" screentip="Connect to an Epiphany server and sign in"/>
                  <button id="signout" label="Sign out" size="large" imageMso="Lock"
                          onAction="OnSignOut" getEnabled="IsConnected"/>
                  <button id="status" label="Status" size="large" imageMso="Info"
                          onAction="OnStatus"/>
                </group>
                <group id="dataGroup" label="Data">
                  <button id="commit" label="Commit selection" size="large" imageMso="DatabaseSaveSelection"
                          onAction="OnCommit" getEnabled="IsConnected"
                          screentip="Write the selected table back to the cube in one transaction"/>
                </group>
                <group id="whatIfGroup" label="What-if">
                  <editBox id="sandbox" label="Sandbox" onChange="OnSandboxChanged" getText="GetSandbox"
                           sizeString="wwwwwwwwwwww"
                           screentip="Name a what-if sandbox to read/write through, or leave blank for base data"/>
                </group>
              </tab>
            </tabs>
          </ribbon>
        </customUI>
        """;

    public void OnLoad(IRibbonUI ribbon) => _ribbon = ribbon;

    public bool IsConnected(IRibbonControl control) => AddIn.Client is not null;

    public void OnConnect(IRibbonControl control)
    {
        try
        {
            var initial = TokenStore.LastBaseUrl() ?? AddIn.Client?.BaseUrl;
            using var form = new ConfiguratorForm(initial);
            form.ShowDialog();
        }
        catch (Exception e)
        {
            MessageBox.Show("Could not open the connect window: " + e.Message, "Epiphany",
                MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
        _ribbon?.Invalidate();
    }

    public void OnSignOut(IRibbonControl control)
    {
        TokenStore.Clear();
        AddIn.Client = null;
        _ribbon?.Invalidate();
        MessageBox.Show("Signed out.", "Epiphany", MessageBoxButtons.OK, MessageBoxIcon.Information);
    }

    public void OnStatus(IRibbonControl control)
        => MessageBox.Show(Functions.Status(), "Epiphany", MessageBoxButtons.OK, MessageBoxIcon.Information);

    public void OnCommit(IRibbonControl control) => CommitService.CommitSelection();

    public void OnSandboxChanged(IRibbonControl control, string text)
    {
        if (AddIn.Client is { } client)
            client.Sandbox = string.IsNullOrWhiteSpace(text) ? null : text.Trim();
    }

    public string GetSandbox(IRibbonControl control) => AddIn.Client?.Sandbox ?? "";
}
