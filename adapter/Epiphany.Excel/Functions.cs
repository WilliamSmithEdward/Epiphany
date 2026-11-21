using System;
using System.Collections.Generic;
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

        [ExcelFunction(Category = Category, Description = "The name signed in to an environment")]
        public static object TM1USER(object environment)
        {
            return AddIn.UserOf(Text(environment)) ?? (object)Connection.KeyError;
        }

        [ExcelFunction(Category = Category, Description = "The number of levels in a dimension")]
        public static object DNLEV(object dimension)
        {
            return Ask(nameof(DNLEV), dimension, found => found.Levels());
        }

        [ExcelFunction(Category = Category, Description = "The element after one in its dimension")]
        public static object DNEXT(object dimension, object element)
        {
            return Ask(nameof(DNEXT), dimension, found => found.Next(Text(element)), element);
        }

        [ExcelFunction(Category = Category, Description = "The type of an element: N, S or C")]
        public static object DTYPE(object dimension, object element)
        {
            return Ask(nameof(DTYPE), dimension, found => found.Type(Text(element)), element);
        }

        [ExcelFunction(Category = Category, Description = "Whether an element is a child of another")]
        public static object ELISCOMP(object dimension, object element, object parent)
        {
            return Ask(nameof(ELISCOMP), dimension,
                found => found.IsChild(Text(element), Text(parent)), element, parent);
        }

        [ExcelFunction(Category = Category, Description = "Whether an element is a parent of another")]
        public static object ELISPAR(object dimension, object element, object child)
        {
            return Ask(nameof(ELISPAR), dimension,
                found => found.IsChild(Text(child), Text(element)), element, child);
        }

        [ExcelFunction(Category = Category, Description = "Whether an element is an ancestor of another")]
        public static object ELISANC(object dimension, object element, object below)
        {
            return Ask(nameof(ELISANC), dimension,
                found => found.IsAncestor(Text(element), Text(below)), element, below);
        }

        [ExcelFunction(Category = Category, Description = "The weight of a child in a consolidation")]
        public static object ELWEIGHT(object dimension, object parent, object child)
        {
            return Ask(nameof(ELWEIGHT), dimension,
                found => found.Weight(Text(parent), Text(child)), parent, child);
        }

        [ExcelFunction(Category = Category, Description = "The number of elements in a subset")]
        public static object SUBSIZ(object dimension, object subset)
        {
            return Ask(nameof(SUBSIZ), dimension, found => found.SubsetSize(Text(subset)), subset);
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
            var name = Text(cube);
            var connection = AddIn.For(ref name);
            var at = elements.Select(Text).ToArray();
            var arguments = new[] { value, cube }.Concat(elements).ToArray();

            return Run(function, arguments, connection,
                () => value == null ? connection.Read(name, at) : connection.Write(name, at, value));
        }

        private static object Ask(string function, object dimension, Func<Dimension, object> question,
            params object[] others)
        {
            var name = Text(dimension);
            var connection = AddIn.For(ref name);
            var arguments = new[] { dimension }.Concat(others).ToArray();
            return Run(function, arguments, connection, () => connection.Ask(name, question));
        }

        // The last answer to each question, shown while the next is on its way. Formulas are
        // calculated on Excel's one calculation thread, so nothing else touches it.
        private static readonly Dictionary<string, object> Last = new Dictionary<string, object>();
        private const int MostKept = 500000;

        /// <summary>Answers a formula once its task is done. Until then the formula shows the
        /// answer it had last, or #GETTING_DATA if it has had none. A server that cannot be
        /// reached is #N/A. An argument that is an error is the answer itself, and an
        /// environment that is not known is a key error.</summary>
        private static object Run(string function, object[] arguments, Connection connection,
            Func<Task<object>> question)
        {
            var error = arguments.FirstOrDefault(argument => argument is ExcelError);
            if (error != null)
                return error;
            if (connection == null)
                return Connection.KeyError;
            // Excel-DNA tells one question from another by function and arguments, and hands a
            // question asked twice the answer it already has. Counting the refreshes among the
            // arguments makes every formula a new question after Refresh.
            var asked = arguments.Concat(new object[] { AddIn.Refreshes }).ToArray();
            var answer = ExcelAsyncUtil.RunTask(function, asked, async () =>
            {
                var given = await question().ConfigureAwait(false);
                // #N/A is what Excel-DNA answers while a task runs, so it cannot be an answer.
                return ReferenceEquals(given, Connection.Unavailable) ? ExcelError.ExcelErrorNull : given;
            });
            // Each argument is made text here and not left to string.Join, which on the .NET
            // Framework gives nothing at all for a list whose first item is null, as a read's
            // is: every read would then share one last answer.
            var texts = arguments.Select(argument => Convert.ToString(argument, CultureInfo.InvariantCulture) ?? "");
            var key = function + "\u001f" + string.Join("\u001f", texts);
            if (Equals(answer, ExcelError.ExcelErrorNA))
                return Last.TryGetValue(key, out var last) ? last : ExcelError.ExcelErrorGettingData;
            if (Equals(answer, ExcelError.ExcelErrorNull))
                answer = ExcelError.ExcelErrorNA;
            if (Last.Count >= MostKept)
                Last.Clear();
            Last[key] = answer;
            return answer;
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
