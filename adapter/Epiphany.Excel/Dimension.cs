using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;

namespace Epiphany.Excel
{
    /// <summary>One element of a dimension, as <c>GET /dimensions/{name}</c> describes it.
    /// What it has none of is one for all: a million leaves each with an empty map of
    /// attributes, list of children and list of weights of its own held some 100 MB.</summary>
    public sealed class Element
    {
        private static readonly Dictionary<string, object> NoValues = new Dictionary<string, object>();

        public string Name;
        public double Level;
        public string[] Parents = Array.Empty<string>();
        public string[] Children = Array.Empty<string>();
        /// <summary>The weight of each child, in the order of <see cref="Children"/>.</summary>
        public double[] Weights = Array.Empty<double>();
        /// <summary>"numeric", "string" or "consolidated".</summary>
        public string Type;
        /// <summary>The values the element has, by attribute key.</summary>
        public Dictionary<string, object> Attributes = NoValues;
    }

    /// <summary>A copy of a dimension, and the answers the dimension functions read from it.
    /// Every answer is text, a number, or <see cref="Connection.KeyError"/>.</summary>
    public sealed class Dimension
    {
        private readonly List<Element> elements;
        // The elements' names, in order: what DIMNM and SUBNM count through, made once, where
        // made at every question a dimension of a million elements took 30 ms a formula.
        private readonly string[] names;
        private readonly double levels;
        // Positions by the key of every name and alias.
        private readonly Dictionary<string, int> positions;
        // "string", "numeric" or "alias", by attribute key.
        private readonly Dictionary<string, string> attributes;
        // Each subset's members, by the subset's key.
        private readonly Dictionary<string, string[]> subsets;
        // Each subset's members as a set, made the first time a member is looked for in it.
        private readonly Dictionary<string, Lazy<HashSet<string>>> held = new Dictionary<string, Lazy<HashSet<string>>>();

        /// <summary>Names are compared as the server compares them: ignoring case and spaces.</summary>
        internal static string Key(string name)
        {
            return new string((name ?? "").Where(c => !char.IsWhiteSpace(c)).ToArray()).ToLowerInvariant();
        }

        /// <summary>A dimension as <c>GET /dimensions/{name}</c> answers, made as it is read.
        /// Text that is not JSON throws <see cref="FormatException"/>.</summary>
        internal static Dimension Read(TextReader text)
        {
            var json = new JsonReader(text);
            var elements = new List<Element>();
            var attributes = new Dictionary<string, string>();
            var subsets = new Dictionary<string, string[]>();
            json.Object();
            while (json.Field(out var field))
            {
                switch (field)
                {
                    case "elements":
                        json.Array();
                        while (json.Item())
                            elements.Add(ReadElement(json));
                        break;
                    case "attributes":
                        json.Object();
                        while (json.Field(out var attribute))
                            attributes[Key(attribute)] = json.Text();
                        break;
                    case "subsets":
                        json.Object();
                        while (json.Field(out var subset))
                            subsets[Key(subset)] = ReadNames(json);
                        break;
                    default:
                        json.Skip();
                        break;
                }
            }
            json.End();
            return new Dimension(elements, attributes, subsets);
        }

        private static Element ReadElement(JsonReader json)
        {
            var element = new Element();
            json.Object();
            while (json.Field(out var field))
            {
                switch (field)
                {
                    case "name":
                        element.Name = json.Text();
                        break;
                    case "type":
                        element.Type = TypeOf(json.Text());
                        break;
                    case "level":
                        element.Level = json.Number();
                        break;
                    case "parents":
                        element.Parents = ReadNames(json);
                        break;
                    case "children":
                        ReadChildren(json, element);
                        break;
                    case "attributes":
                        var values = new Dictionary<string, object>();
                        json.Object();
                        while (json.Field(out var attribute))
                            values[Key(attribute)] = Json.Scalar(json.Value());
                        if (values.Count > 0)
                            element.Attributes = values;
                        break;
                    default:
                        json.Skip();
                        break;
                }
            }
            return element;
        }

        private static void ReadChildren(JsonReader json, Element element)
        {
            var children = new List<string>();
            var weights = new List<double>();
            json.Array();
            while (json.Item())
            {
                string name = null;
                var weight = 0.0;
                json.Object();
                while (json.Field(out var field))
                {
                    if (field == "name")
                        name = json.Text();
                    else if (field == "weight")
                        weight = json.Number();
                    else
                        json.Skip();
                }
                children.Add(name);
                weights.Add(weight);
            }
            if (children.Count == 0)
                return;
            element.Children = children.ToArray();
            element.Weights = weights.ToArray();
        }

        private static string[] ReadNames(JsonReader json)
        {
            var items = new List<string>();
            json.Array();
            while (json.Item())
                items.Add(json.Text());
            return items.Count == 0 ? Array.Empty<string>() : items.ToArray();
        }

        private Dimension(List<Element> elements, Dictionary<string, string> attributes, Dictionary<string, string[]> subsets)
        {
            this.elements = elements;
            this.attributes = attributes;
            this.subsets = subsets;
            foreach (var subset in subsets)
            {
                var members = subset.Value;
                held[subset.Key] = new Lazy<HashSet<string>>(() => new HashSet<string>(members));
            }
            levels = elements.Count == 0 ? 0.0 : elements.Max(element => element.Level) + 1;
            // Made as large as they will be at once: grown a name at a time, both were made again
            // larger and larger, and a million names took the add-in's memory 40 to 55 MB higher.
            names = new string[elements.Count];
            positions = new Dictionary<string, int>(elements.Count);
            // Names first, so that a name always wins over another element's alias.
            for (var position = 0; position < elements.Count; position++)
            {
                names[position] = elements[position].Name;
                positions[Key(names[position])] = position;
            }
            for (var position = 0; position < elements.Count; position++)
                foreach (var value in elements[position].Attributes)
                    if (attributes[value.Key] == "alias" && !positions.ContainsKey(Key((string)value.Value)))
                        positions[Key((string)value.Value)] = position;
        }

        /// <summary>The type, one string for every element of it.</summary>
        private static string TypeOf(string type)
        {
            switch (type)
            {
                case "numeric": return "numeric";
                case "string": return "string";
                case "consolidated": return "consolidated";
                default: return type;
            }
        }

        /// <summary>DNLEV: how many levels the dimension has, the leaves' included.</summary>
        public object Levels()
        {
            return levels;
        }

        /// <summary>DNEXT: the element after this one in the dimension, or empty text.</summary>
        public object Next(string element)
        {
            return positions.TryGetValue(Key(element), out var position)
                ? At(names, position + 2.0)
                : Connection.KeyError;
        }

        /// <summary>DTYPE: N for a numeric element, S for a string, C for a consolidation.</summary>
        public object Type(string element)
        {
            return With(element, found => found.Type.Substring(0, 1).ToUpperInvariant());
        }

        /// <summary>ELISCOMP: whether an element is a child of another.</summary>
        public object IsChild(string element, string parent)
        {
            return With(element, child => With(parent, found => found.Children.Contains(child.Name)));
        }

        /// <summary>ELISANC: whether an element is above another, at any distance.</summary>
        public object IsAncestor(string element, string below)
        {
            return With(element, ancestor => With(below, found =>
            {
                // Up by every parent, for an element may have several.
                var waiting = new Stack<string>(found.Parents);
                var seen = new HashSet<string>();
                while (waiting.Count > 0)
                {
                    var name = waiting.Pop();
                    if (name == ancestor.Name)
                        return true;
                    if (seen.Add(name))
                        foreach (var parent in elements[positions[Key(name)]].Parents)
                            waiting.Push(parent);
                }
                return (object)false;
            }));
        }

        /// <summary>ELWEIGHT: the weight of a child in a consolidation, or zero if it is not
        /// a child of it.</summary>
        public object Weight(string parent, string child)
        {
            return With(parent, found => With(child, other =>
            {
                var at = Array.IndexOf(found.Children, other.Name);
                return at < 0 ? 0.0 : found.Weights[at];
            }));
        }

        /// <summary>SUBSIZ: how many elements a subset holds.</summary>
        public object SubsetSize(string subset)
        {
            return subsets.TryGetValue(Key(subset), out var members) ? (double)members.Length : (object)Connection.KeyError;
        }

        /// <summary>DIMSIZ.</summary>
        public object Size()
        {
            return (double)elements.Count;
        }

        /// <summary>DIMNM.</summary>
        public object Name(double index, string alias)
        {
            return Shown(At(names, index), alias);
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
            var members = names;
            var whole = Key(subset) == "";
            if (!whole && !subsets.TryGetValue(Key(subset), out members))
                return Connection.KeyError;
            if (indexOrElement is double index)
                return Shown(At(members, index), alias);
            var name = positions.TryGetValue(Key(indexOrElement as string), out var position)
                ? elements[position].Name
                : "";
            // Every element is a member of the whole dimension; a subset's, of the set made of it.
            var member = name != "" && (whole || held[Key(subset)].Value.Contains(name));
            return Shown(member ? name : "", alias);
        }

        /// <summary>The elements SUBNM chooses among, each with what it is shown as: its name,
        /// or its alias when an alias attribute is named.</summary>
        public object Members(string subset, string alias)
        {
            var members = names;
            if (Key(subset) != "" && !subsets.TryGetValue(Key(subset), out members))
                return Connection.KeyError;
            if (Key(alias) != "" && (!attributes.TryGetValue(Key(alias), out var kind) || kind != "alias"))
                return Connection.KeyError;
            return members
                .Select(name => new KeyValuePair<string, string>(name, (string)Shown(name, alias)))
                .ToList();
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
