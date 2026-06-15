using System.Globalization;
using System.Windows.Forms;
using ExcelDna.Integration;

namespace Epiphany.ExcelAddIn;

/// <summary>
/// Write-back (ADR-0022). A UDF cannot write to other cells, so committing is a
/// deliberate, range-based action: the user selects a small table and clicks
/// Commit. The table's first row is a header - a dimension name per leading
/// column and "Value" as the last column - and each following row is one
/// coordinate plus the value to write. The whole selection becomes ONE
/// transactional <c>cells/batch</c> POST (all-or-nothing, server-validated).
/// </summary>
public static class CommitService
{
    public static void CommitSelection()
    {
        if (AddIn.Client is null)
        {
            MessageBox.Show("Not connected. Use the Epiphany ribbon to Connect first.",
                "Epiphany", MessageBoxButtons.OK, MessageBoxIcon.Information);
            return;
        }

        object[,]? grid;
        try
        {
            grid = ReadSelection();
        }
        catch (Exception e)
        {
            MessageBox.Show("Could not read the selection: " + e.Message, "Epiphany",
                MessageBoxButtons.OK, MessageBoxIcon.Warning);
            return;
        }
        if (grid is null)
        {
            MessageBox.Show(
                "Select a table to commit: the first row names the dimensions with \"Value\" as the last column, and each following row is one coordinate and its value.",
                "Epiphany", MessageBoxButtons.OK, MessageBoxIcon.Information);
            return;
        }

        List<CellWrite> writes;
        try
        {
            writes = BuildWrites(grid);
        }
        catch (Exception e)
        {
            MessageBox.Show(e.Message, "Epiphany", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            return;
        }
        if (writes.Count == 0)
        {
            MessageBox.Show("Nothing to write (no data rows with a value).", "Epiphany",
                MessageBoxButtons.OK, MessageBoxIcon.Information);
            return;
        }

        using var prompt = new CubePromptForm();
        if (prompt.ShowDialog() != DialogResult.OK || string.IsNullOrWhiteSpace(prompt.Cube))
            return;

        try
        {
            var applied = AddIn.Client.BatchWriteAsync(prompt.Cube, writes).GetAwaiter().GetResult();
            MessageBox.Show($"Committed {applied} cell(s) to \"{prompt.Cube}\".", "Epiphany",
                MessageBoxButtons.OK, MessageBoxIcon.Information);
        }
        catch (Exception e)
        {
            MessageBox.Show("Commit failed (nothing was written): " + e.Message, "Epiphany",
                MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
    }

    /// <summary>Read the active selection as a 1-based 2D object grid, or null if not a table.</summary>
    private static object[,]? ReadSelection()
    {
        dynamic app = ExcelDnaUtil.Application;
        dynamic selection = app.Selection;
        int rows = (int)selection.Rows.Count;
        int cols = (int)selection.Columns.Count;
        if (rows < 2 || cols < 2) return null;
        // Value2 of a multi-cell range is a 1-based object[,].
        return (object[,])selection.Value2;
    }

    internal static List<CellWrite> BuildWrites(object[,] grid)
    {
        int rows = grid.GetLength(0);
        int cols = grid.GetLength(1);
        // 1-based bounds from Excel.
        int rLo = grid.GetLowerBound(0), cLo = grid.GetLowerBound(1);

        var headers = new string[cols];
        for (int c = 0; c < cols; c++)
            headers[c] = Convert.ToString(grid[rLo, cLo + c], CultureInfo.InvariantCulture)?.Trim() ?? "";

        int valueCol = Array.FindLastIndex(headers, h => h.Equals("Value", StringComparison.OrdinalIgnoreCase));
        if (valueCol < 0) valueCol = cols - 1; // fall back to the last column
        if (valueCol == 0)
            throw new FormatException("The first row needs dimension columns and a Value column.");

        var writes = new List<CellWrite>();
        for (int r = 1; r < rows; r++)
        {
            var raw = grid[rLo + r, cLo + valueCol];
            if (raw is null) continue;
            var valueText = ToDecimalString(raw);
            if (valueText is null) continue; // blank value -> skip this row

            var coord = new Dictionary<string, string>();
            for (int c = 0; c < cols; c++)
            {
                if (c == valueCol || headers[c].Length == 0) continue;
                var member = Convert.ToString(grid[rLo + r, cLo + c], CultureInfo.InvariantCulture)?.Trim();
                if (string.IsNullOrEmpty(member)) continue;
                coord[headers[c]] = member;
            }
            if (coord.Count > 0)
                writes.Add(new CellWrite(coord, valueText));
        }
        return writes;
    }

    /// <summary>Format a cell value as the decimal/text string the API expects, or null if blank.</summary>
    internal static string? ToDecimalString(object raw)
    {
        switch (raw)
        {
            case null:
                return null;
            case double d:
                return d.ToString("R", CultureInfo.InvariantCulture);
            case string s:
                return s.Trim().Length == 0 ? null : s.Trim();
            default:
                var t = Convert.ToString(raw, CultureInfo.InvariantCulture)?.Trim();
                return string.IsNullOrEmpty(t) ? null : t;
        }
    }
}

/// <summary>A small dialog to pick the target cube, populated from the server.</summary>
internal sealed class CubePromptForm : Form
{
    private readonly ComboBox _combo = new();
    public string? Cube => _combo.Text?.Trim();

    public CubePromptForm()
    {
        Text = "Commit to cube";
        Width = 360;
        Height = 150;
        FormBorderStyle = FormBorderStyle.FixedDialog;
        StartPosition = FormStartPosition.CenterScreen;
        MaximizeBox = false;
        MinimizeBox = false;

        var label = new Label { Text = "Cube:", Left = 12, Top = 16, Width = 60 };
        _combo.Left = 76;
        _combo.Top = 12;
        _combo.Width = 256;
        _combo.DropDownStyle = ComboBoxStyle.DropDown;

        var ok = new Button { Text = "Commit", Left = 176, Top = 60, Width = 75, DialogResult = DialogResult.OK };
        var cancel = new Button { Text = "Cancel", Left = 257, Top = 60, Width = 75, DialogResult = DialogResult.Cancel };
        AcceptButton = ok;
        CancelButton = cancel;

        Controls.Add(label);
        Controls.Add(_combo);
        Controls.Add(ok);
        Controls.Add(cancel);

        Load += async (_, _) =>
        {
            try
            {
                if (AddIn.Client is { } client)
                {
                    var cubes = await client.ListCubesAsync();
                    foreach (var c in cubes) _combo.Items.Add(c);
                    if (_combo.Items.Count > 0) _combo.SelectedIndex = 0;
                }
            }
            catch
            {
                // leave the combo editable; the user can type a cube name
            }
        };
    }
}
