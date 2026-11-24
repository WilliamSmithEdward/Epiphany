using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text;
using System.Web.Script.Serialization;

namespace Epiphany.Excel
{
    /// <summary>JSON written by the serializer that ships with the .NET Framework, and read by
    /// <see cref="JsonReader"/>, so that the add-in carries no libraries of its own.</summary>
    internal static class Json
    {
        public static string Write(object value)
        {
            return new JavaScriptSerializer { MaxJsonLength = int.MaxValue }.Serialize(value);
        }

        /// <summary>The value the text holds, or null where there is no text. Objects come back
        /// as dictionaries, arrays as object arrays, whole numbers as longs and other numbers
        /// as doubles.</summary>
        public static object Read(TextReader text)
        {
            if (text.Peek() < 0)
                return null;
            var json = new JsonReader(text);
            var value = json.Value();
            json.End();
            return value;
        }

        public static object Read(string text)
        {
            return Read(new StringReader(text));
        }

        public static object Field(object json, string name)
        {
            object value = null;
            (json as Dictionary<string, object>)?.TryGetValue(name, out value);
            return value;
        }

        public static object[] Items(object json)
        {
            return json as object[] ?? Array.Empty<object>();
        }

        /// <summary>A cell or attribute value as Excel takes it: text, or a number as a double.</summary>
        public static object Scalar(object json)
        {
            return json is string ? json : Convert.ToDouble(json);
        }
    }

    /// <summary>JSON read a value at a time as the text comes, so that what it holds can be made
    /// into what it is for as it is read: made into dictionaries first, a dimension of a million
    /// elements took the framework's serializer 9 s and 2 GB. An object is read by opening it
    /// and then reading each field's value as <see cref="Field"/> names it, an array by opening
    /// it and reading an item while <see cref="Item"/> says there is one. Text that is not JSON
    /// throws <see cref="FormatException"/>.</summary>
    internal sealed class JsonReader
    {
        private readonly TextReader text;
        private readonly char[] buffer = new char[1 << 16];
        private int at;
        private int length;
        // Text and numbers gathered a character at a time.
        private readonly StringBuilder scratch = new StringBuilder();
        // An object or array has just been opened: its first field or item has no comma before it.
        private bool opened;

        public JsonReader(TextReader text)
        {
            this.text = text;
        }

        /// <summary>Any value: objects as dictionaries, arrays as object arrays, whole numbers as
        /// longs and other numbers as doubles. A whole number is kept whole, as the framework's
        /// serializer kept it, for it may go back to the server: a transaction's id made a double
        /// was written 1.23456789012346E+15.</summary>
        public object Value()
        {
            switch (Peek())
            {
                case '{':
                    var fields = new Dictionary<string, object>();
                    Object();
                    while (Field(out var name))
                        fields[name] = Value();
                    return fields;
                case '[':
                    var items = new List<object>();
                    Array();
                    while (Item())
                        items.Add(Value());
                    return items.ToArray();
                case '"':
                    return Text();
                case 't':
                    return Word("true", true);
                case 'f':
                    return Word("false", false);
                case 'n':
                    return Word("null", null);
                default:
                    var number = Numeral();
                    return long.TryParse(number, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var whole)
                        ? whole
                        : (object)Parsed(number);
            }
        }

        /// <summary>A value of no use to the caller.</summary>
        public void Skip()
        {
            Value();
        }

        /// <summary>Opens an object.</summary>
        public void Object()
        {
            Expect('{');
            opened = true;
        }

        /// <summary>Opens an array.</summary>
        public void Array()
        {
            Expect('[');
            opened = true;
        }

        /// <summary>The name of the object's next field, whose value is to be read next; false
        /// once the object has closed.</summary>
        public bool Field(out string name)
        {
            name = null;
            if (!Next('}'))
                return false;
            name = Text();
            Expect(':');
            return true;
        }

        /// <summary>Whether the array has another item, to be read next; false once it has
        /// closed.</summary>
        public bool Item()
        {
            return Next(']');
        }

        public string Text()
        {
            Expect('"');
            // Most text has no escapes, and lies whole within what has been read: made at once.
            for (var end = at; end < length && buffer[end] != '\\'; end++)
            {
                if (buffer[end] == '"')
                {
                    var whole = new string(buffer, at, end - at);
                    at = end + 1;
                    return whole;
                }
            }
            scratch.Clear();
            for (var c = Take(); c != '"'; c = Take())
                scratch.Append(c == '\\' ? Escaped() : c);
            return scratch.ToString();
        }

        public double Number()
        {
            return Parsed(Numeral());
        }

        // A number as it is written.
        private string Numeral()
        {
            Peek();
            scratch.Clear();
            while ((at < length || Fill()) && Numeric(buffer[at]))
                scratch.Append(buffer[at++]);
            return scratch.ToString();
        }

        private static double Parsed(string number)
        {
            if (!double.TryParse(number, NumberStyles.Float, CultureInfo.InvariantCulture, out var parsed))
                throw Malformed();
            return parsed;
        }

        /// <summary>Throws unless nothing but white space is left.</summary>
        public void End()
        {
            if (Peek() >= 0)
                throw Malformed();
        }

        // The character an escape stands for, its backslash read. A character beyond the basic
        // plane is escaped as its two UTF-16 halves, each read as a character of its own.
        private char Escaped()
        {
            var c = Take();
            switch (c)
            {
                case '"':
                case '\\':
                case '/':
                    return c;
                case 'b':
                    return '\b';
                case 'f':
                    return '\f';
                case 'n':
                    return '\n';
                case 'r':
                    return '\r';
                case 't':
                    return '\t';
                case 'u':
                    var code = 0;
                    for (var digit = 0; digit < 4; digit++)
                        code = code * 16 + Hex(Take());
                    return (char)code;
                default:
                    throw Malformed();
            }
        }

        private static int Hex(char c)
        {
            if (c >= '0' && c <= '9')
                return c - '0';
            if (c >= 'a' && c <= 'f')
                return c - 'a' + 10;
            if (c >= 'A' && c <= 'F')
                return c - 'A' + 10;
            throw Malformed();
        }

        private static bool Numeric(char c)
        {
            return (c >= '0' && c <= '9') || c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E';
        }

        private object Word(string word, object value)
        {
            foreach (var c in word)
            {
                if (Take() != c)
                    throw Malformed();
            }
            return value;
        }

        private void Expect(char c)
        {
            if (Peek() != c)
                throw Malformed();
            at++;
        }

        // Whether another field or item follows, the comma before it read; false at the bracket
        // that closes them, which is read.
        private bool Next(char close)
        {
            var c = Peek();
            var first = opened;
            opened = false;
            if (c == close)
            {
                at++;
                return false;
            }
            if (first)
                return true;
            if (c != ',')
                throw Malformed();
            at++;
            return true;
        }

        // The next character that is not white space, left to be taken; -1 at the end.
        private int Peek()
        {
            for (; at < length || Fill(); at++)
            {
                var c = buffer[at];
                if (c != ' ' && c != '\n' && c != '\r' && c != '\t')
                    return c;
            }
            return -1;
        }

        // The next character, white space or not.
        private char Take()
        {
            if (at == length && !Fill())
                throw Malformed();
            return buffer[at++];
        }

        private bool Fill()
        {
            length = text.Read(buffer, 0, buffer.Length);
            at = 0;
            return length > 0;
        }

        private static FormatException Malformed()
        {
            return new FormatException("not JSON");
        }
    }
}
