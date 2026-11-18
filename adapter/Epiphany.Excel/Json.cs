using System;
using System.Collections.Generic;
using System.Web.Script.Serialization;

namespace Epiphany.Excel
{
    /// <summary>JSON by the serializer that ships with the .NET Framework, so that the add-in
    /// carries no libraries of its own.</summary>
    internal static class Json
    {
        private static JavaScriptSerializer Serializer()
        {
            return new JavaScriptSerializer { MaxJsonLength = int.MaxValue };
        }

        public static string Write(object value)
        {
            return Serializer().Serialize(value);
        }

        /// <summary>Objects come back as dictionaries, arrays as object arrays.</summary>
        public static object Read(string text)
        {
            return Serializer().DeserializeObject(text);
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
}
