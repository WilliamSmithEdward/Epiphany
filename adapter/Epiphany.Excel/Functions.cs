using System;
using System.Globalization;
using System.Linq;
using System.Threading.Tasks;
using ExcelDna.Integration;

namespace Epiphany.Excel
{
    /// <summary>The worksheet functions. Each one only turns its arguments into a question for
    /// the <see cref="Connection"/>; docs/PAFE-COMPATIBILITY.md says what each answers.</summary>
    public static class Functions
    {
        private const string Category = "Epiphany";

        [ExcelFunction(Category = Category, Description = "Reads a cube cell")]
        public static object DBRW(object cube, params object[] elements)
        {
            return Cell(nameof(DBRW), null, cube, elements);
        }

        [ExcelFunction(Category = Category, Description = "Reads a cube cell")]
        public static object DBR(object cube, params object[] elements)
        {
            return Cell(nameof(DBR), null, cube, elements);
        }

        [ExcelFunction(Category = Category, Description = "Writes a number to a cube cell")]
        public static object DBS(double value, object cube, params object[] elements)
        {
            return Cell(nameof(DBS), value, cube, elements);
        }

        [ExcelFunction(Category = Category, Description = "Writes a number to a cube cell")]
        public static object DBSW(double value, object cube, params object[] elements)
        {
            return Cell(nameof(DBSW), value, cube, elements);
        }

        [ExcelFunction(Category = Category, Description = "Writes text to a cube cell")]
        public static object DBSS(string value, object cube, params object[] elements)
        {
            return Cell(nameof(DBSS), value ?? "", cube, elements);
        }

        [ExcelFunction(Category = Category, Description = "The element at an index of a dimension")]
        public static object DIMNM(object dimension, object index, object alias)
        {
            return Ask(nameof(DIMNM), dimension, found => found.Name(Number(index), Text(alias)), index, alias);
        }

        [ExcelFunction(Category = Category, Description = "The index of an element in its dimension")]
        public static object DIMIX(object dimension, object element)
        {
            return Ask(nameof(DIMIX), dimension, found => found.Index(Text(element)), element);
        }

        [ExcelFunction(Category = Category, Description = "The number of elements in a dimension")]
        public static object DIMSIZ(object dimension)
        {
            return Ask(nameof(DIMSIZ), dimension, found => found.Size());
        }

        [ExcelFunction(Category = Category, Description = "The level of an element; a leaf is 0")]
        public static object ELLEV(object dimension, object element)
        {
            return Ask(nameof(ELLEV), dimension, found => found.Level(Text(element)), element);
        }

        [ExcelFunction(Category = Category, Description = "The parent of an element at an index")]
        public static object ELPAR(object dimension, object element, object index)
        {
            return Ask(nameof(ELPAR), dimension, found => found.Parent(Text(element), Number(index)), element, index);
        }

        [ExcelFunction(Category = Category, Description = "The number of parents of an element")]
        public static object ELPARN(object dimension, object element)
        {
            return Ask(nameof(ELPARN), dimension, found => found.ParentCount(Text(element)), element);
        }

        [ExcelFunction(Category = Category, Description = "The child of an element at an index")]
        public static object ELCOMP(object dimension, object element, object index)
        {
            return Ask(nameof(ELCOMP), dimension, found => found.Child(Text(element), Number(index)), element, index);
        }

        [ExcelFunction(Category = Category, Description = "The number of children of an element")]
        public static object ELCOMPN(object dimension, object element)
        {
            return Ask(nameof(ELCOMPN), dimension, found => found.ChildCount(Text(element)), element);
        }

        [ExcelFunction(Category = Category, Description = "An attribute of an element")]
        public static object DBRA(object dimension, object element, object attribute)
        {
            return Ask(nameof(DBRA), dimension,
                found => found.Attribute(Text(element), Text(attribute)), element, attribute);
        }

        [ExcelFunction(Category = Category, Description = "An attribute of an element, as text")]
        public static object ATTRS(object dimension, object element, object attribute)
        {
            return Ask(nameof(ATTRS), dimension,
                found => found.AttributeText(Text(element), Text(attribute)), element, attribute);
        }

        [ExcelFunction(Category = Category, Description = "A numeric attribute of an element")]
        public static object ATTRN(object dimension, object element, object attribute)
        {
            return Ask(nameof(ATTRN), dimension,
                found => found.AttributeNumber(Text(element), Text(attribute)), element, attribute);
        }

        [ExcelFunction(Category = Category, Description = "An element of a subset, by index or by name")]
        public static object SUBNM(object dimension, object subset, object indexOrElement, object alias)
        {
            var chosen = indexOrElement is double ? indexOrElement : Text(indexOrElement);
            return Ask(nameof(SUBNM), dimension,
                found => found.SubsetName(Text(subset), chosen, Text(alias)), subset, indexOrElement, alias);
        }

        /// <summary>Reads a cell, or writes it when there is a value.</summary>
        private static object Cell(string function, object value, object cube, object[] elements)
        {
            var connection = AddIn.Connection;
            // PAfE writes a cube as server:cube; there is one server here.
            var name = Text(cube);
            name = name.Substring(name.IndexOf(':') + 1);
            var at = elements.Select(Text).ToArray();
            var arguments = new[] { value, cube }.Concat(elements).ToArray();
            return Run(function, arguments,
                () => value == null ? connection.Read(name, at) : connection.Write(name, at, value));
        }

        private static object Ask(string function, object dimension, Func<Dimension, object> question,
            params object[] others)
        {
            var connection = AddIn.Connection;
            var arguments = new[] { dimension }.Concat(others).ToArray();
            return Run(function, arguments, () => connection.Ask(Text(dimension), question));
        }

        /// <summary>Answers a formula once its task is done. Until then, and whenever the
        /// formula is calculated again with the same arguments while the task runs, Excel
        /// shows #N/A. An argument that is an error is the answer itself.</summary>
        private static object Run(string function, object[] arguments, Func<Task<object>> question)
        {
            var error = arguments.FirstOrDefault(argument => argument is ExcelError);
            if (error != null)
                return error;
            return ExcelAsyncUtil.RunTask(function, arguments, async () =>
            {
                var answer = await question().ConfigureAwait(false);
                return ReferenceEquals(answer, Connection.Unavailable) ? ExcelError.ExcelErrorNA : answer;
            });
        }

        /// <summary>An argument as a name. Numbers are names too: a year, say.</summary>
        private static string Text(object argument)
        {
            switch (argument)
            {
                case string text: return text;
                case double number: return number.ToString("R", CultureInfo.InvariantCulture);
                case bool flag: return flag ? "TRUE" : "FALSE";
                default: return "";
            }
        }

        /// <summary>An argument as an index; anything that is not a number is no index.</summary>
        private static double Number(object argument)
        {
            if (argument is double number)
                return number;
            double.TryParse(argument as string, NumberStyles.Float, CultureInfo.InvariantCulture, out number);
            return number;
        }
    }
}
