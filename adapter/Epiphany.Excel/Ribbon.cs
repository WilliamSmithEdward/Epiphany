using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using ExcelDna.Integration;
using ExcelDna.Integration.CustomUI;

namespace Epiphany.Excel
{
    /// <summary>The Epiphany tab of Excel's ribbon: a button for each command of the add-in.
    /// The commands themselves are in <see cref="AddIn"/>, where the menu has them too.</summary>
    [ComVisible(true)]
    public class Ribbon : ExcelRibbon
    {
        // By the tag of the button that runs it.
        private static readonly Dictionary<string, ExcelAction> Commands = new Dictionary<string, ExcelAction>
        {
            ["connect"] = AddIn.Connect,
            ["disconnect"] = AddIn.Disconnect,
            ["refresh"] = AddIn.Refresh,
            ["choose"] = AddIn.Choose,
            ["begin"] = AddIn.BeginTransaction,
            ["commit"] = AddIn.CommitTransaction,
            ["rollback"] = AddIn.RollbackTransaction,
        };

        public override string GetCustomUI(string ribbonId)
        {
            return @"
<customUI xmlns='http://schemas.microsoft.com/office/2009/07/customui'>
  <ribbon>
    <tabs>
      <tab id='epiphany' label='Epiphany'>
        <group id='epiphanyServer' label='Server'>
          <button id='epiphanyConnect' label='Connect' size='large' imageMso='Connections' tag='connect' onAction='Run'
                  screentip='Connect to a server' supertip='Name an environment and its server, and sign in. Formulas reach it as environment:cube.'/>
          <button id='epiphanyDisconnect' label='Disconnect' tag='disconnect' onAction='Run'
                  screentip='Sign out of every environment'/>
        </group>
        <group id='epiphanyData' label='Data'>
          <button id='epiphanyRefresh' label='Refresh' size='large' imageMso='RefreshAll' tag='refresh' onAction='Run'
                  screentip='Fetch every value again (Ctrl+Shift+R)'/>
          <button id='epiphanyChoose' label='Choose element' tag='choose' onAction='Run'
                  screentip='Choose the element of the selected SUBNM cell' supertip='A double click on the cell does the same.'/>
        </group>
        <group id='epiphanyTransaction' label='Transaction'>
          <button id='epiphanyBegin' label='Begin' tag='begin' onAction='Run'
                  screentip='Begin a transaction' supertip='What you enter from then on shows in your Excel alone, until you commit.'/>
          <button id='epiphanyCommit' label='Commit' imageMso='FileSave' tag='commit' onAction='Run'
                  screentip='Store what the transaction holds, for everyone'/>
          <button id='epiphanyRollback' label='Roll back' imageMso='Undo' tag='rollback' onAction='Run'
                  screentip='Discard what the transaction holds'/>
        </group>
      </tab>
    </tabs>
  </ribbon>
</customUI>";
        }

        /// <summary>Runs the command of the button pressed, as a macro, which is where a
        /// command may use Excel as it needs to.</summary>
        public void Run(IRibbonControl control)
        {
            if (Commands.TryGetValue(control.Tag, out var command))
                ExcelAsyncUtil.QueueAsMacro(command);
        }
    }
}
