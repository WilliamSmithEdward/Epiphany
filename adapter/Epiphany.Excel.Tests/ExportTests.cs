using System.Linq;
using System.Reflection;
using ExcelDna.Integration;
using Xunit;

namespace Epiphany.Excel.Tests
{
    /// <summary>What the add-in offers Excel.</summary>
    public sealed class ExportTests
    {
        [Fact]
        public void ExcelIsOfferedOnlyTheFunctionsAndCommandsMeantForIt()
        {
            // Excel-DNA offers every public static method of a public type as a worksheet
            // function, marked or not: a helper left public shows in every workbook.
            var unmarked = typeof(Functions).Assembly.GetExportedTypes()
                .SelectMany(type => type.GetMethods(BindingFlags.Public | BindingFlags.Static | BindingFlags.DeclaredOnly))
                .Where(method => !method.IsSpecialName)
                .Where(method => method.GetCustomAttribute<ExcelFunctionAttribute>() == null
                    && method.GetCustomAttribute<ExcelCommandAttribute>() == null)
                .Select(method => method.DeclaringType.Name + "." + method.Name);
            Assert.Empty(unmarked);
            Assert.Equal(26, typeof(Functions).GetMethods().Count(method => method.GetCustomAttribute<ExcelFunctionAttribute>() != null));
        }
    }
}
