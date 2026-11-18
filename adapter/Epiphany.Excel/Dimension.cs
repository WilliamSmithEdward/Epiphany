using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;

namespace Epiphany.Excel
{
    /// <summary>One element of a dimension, as <c>GET /dimensions/{name}</c> describes it.</summary>
    public sealed class Element
    {
        public string Name;
        public double Level;
        public string[] Parents;
        public string[] Children;
        /// <summary>The values the element has, by attribute key.</summary>
        public Dictionary<string, object> Attributes;
    }

    /// <summary>A copy of a dimension, and the answers the dimension functions read from it.
    /// Every answer is text, a number, or <see cref="Connection.KeyError"/>.</summary>
    public sealed class Dimension
    {
        private readonly List<Element> elements = new List<Element>();
        // Positions by the key of every name and alias.
        private readonly Dictionary<string, int> positions = new Dictionary<string, int>();
        // "string", "numeric" or "alias", by attribute key.
        private readonly Dictionary<string, string> attributes = new Dictionary<string, string>();
        private readonly Dictionary<string, string[]> subsets = new Dictionary<string, string[]>();

        /// <summary>Names are compared as the server compares them: ignoring case and spaces.</summary>
        public static string Key(string name)
        {
            return new string((name ?? "").Where(c => !char.IsWhiteSpace(c)).ToArray()).ToLowerInvariant();
        }

        public Dimension(object json)
        {
            foreach (var attribute in Fields(json, "attributes"))
                attributes[Key(attribute.Key)] = (string)attribute.Value;
            foreach (var subset in Fields(json, "subsets"))
                subsets[Key(subset.Key)] = Names(subset.Value);
            foreach (var item in Json.Items(Json.Field(json, "elements")))
            {
                var children = Json.Items(Json.Field(item, "children"));
                elements.Add(new Element
                {
                    Name = (string)Json.Field(item, "name"),
                    Level = Convert.ToDouble(Json.Field(item, "level")),
                    Parents = Names(Json.Field(item, "parents")),
                    Children = children.Select(child => (string)Json.Field(child, "name")).ToArray(),
                    Attributes = Fields(item, "attributes")
                        .ToDictionary(value => Key(value.Key), value => Json.Scalar(value.Value)),
                });
            }
            // Names first, so that a name always wins over another element's alias.
            for (var position = 0; position < elements.Count; position++)
                positions[Key(elements[position].Name)] = position;
            for (var position = 0; position < elements.Count; position++)
                foreach (var value in elements[position].Attributes)
                    if (attributes[value.Key] == "alias" && !positions.ContainsKey(Key((string)value.Value)))
                        positions[Key((string)value.Value)] = position;
        }

        private static Dictionary<string, object> Fields(object json, string name)
        {
            return Json.Field(json, name) as Dictionary<string, object> ?? new Dictionary<string, object>();
        }

        private static string[] Names(object json)
        {
            return Json.Items(json).Cast<string>().ToArray();
        }

        /// <summary>DIMSIZ.</summary>
        public object Size()
        {
            return (double)elements.Count;
        }

        /// <summary>DIMNM.</summary>
        public object Name(double index, string alias)
        {
            return Shown(At(elements.Select(element => element.Name).ToArray(), index), alias);
        }

        /// <summary>DIMIX: zero when there is no such element.</summary>
        public object Index(string element)
        {
            return positions.TryGetValue(Key(element), out var position) ? position + 1.0 : 0.0;
        }

        /// <summary>ELLEV.</summary>
        public object Level(string element)
        {
            return With(element, found => found.Level);
        }

        /// <summary>ELPAR.</summary>
        public object Parent(string element, double index)
        {
            return With(element, found => At(found.Parents, index));
        }

        /// <summary>ELPARN.</summary>
        public object ParentCount(string element)
        {
            return With(element, found => (double)found.Parents.Length);
        }

        /// <summary>ELCOMP.</summary>
        public object Child(string element, double index)
        {
            return With(element, found => At(found.Children, index));
        }

        /// <summary>ELCOMPN.</summary>
        public object ChildCount(string element)
        {
            return With(element, found => (double)found.Children.Length);
        }

        /// <summary>DBRA: the value as it is. An attribute without a value is empty text or
        /// zero, and an alias without a value is the element's name.</summary>
        public object Attribute(string element, string attribute)
        {
            return With(element, found =>
            {
                if (!attributes.TryGetValue(Key(attribute), out var kind))
                    return Connection.KeyError;
                if (found.Attributes.TryGetValue(Key(attribute), out var value))
                    return value;
                return kind == "numeric" ? 0.0 : kind == "alias" ? found.Name : (object)"";
            });
        }

        /// <summary>ATTRS.</summary>
        public object AttributeText(string element, string attribute)
        {
            var value = Attribute(element, attribute);
            return value is double number ? number.ToString("R", CultureInfo.InvariantCulture) : value;
        }

        /// <summary>ATTRN.</summary>
        public object AttributeNumber(string element, string attribute)
        {
            var value = Attribute(element, attribute);
            return value is double ? value : Connection.KeyError;
        }

        /// <summary>SUBNM: the subset's element at an index, or the named element if the subset
        /// holds it. An empty subset name means every element.</summary>
        public object SubsetName(string subset, object indexOrElement, string alias)
        {
            var members = elements.Select(element => element.Name).ToArray();
            if (Key(subset) != "" && !subsets.TryGetValue(Key(subset), out members))
                return Connection.KeyError;
            if (indexOrElement is double index)
                return Shown(At(members, index), alias);
            var name = positions.TryGetValue(Key(indexOrElement as string), out var position)
                ? elements[position].Name
                : "";
            return Shown(members.Contains(name) ? name : "", alias);
        }

        /// <summary>The name at a position counted from one, or empty text beyond either end.</summary>
        private static string At(string[] names, double index)
        {
            return index >= 1 && index < names.Length + 1 ? names[(int)index - 1] : "";
        }

        private object With(string element, Func<Element, object> answer)
        {
            return positions.TryGetValue(Key(element), out var position)
                ? answer(elements[position])
                : Connection.KeyError;
        }

        /// <summary>An element's name, or its alias when an alias attribute is named.</summary>
        private object Shown(string name, string alias)
        {
            if (Key(alias) == "")
                return name;
            if (!attributes.TryGetValue(Key(alias), out var kind) || kind != "alias")
                return Connection.KeyError;
            return name == "" ? name : Attribute(name, alias);
        }
    }
}
