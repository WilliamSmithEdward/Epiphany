using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using Xunit;

namespace Epiphany.Excel.Tests
{
    /// <summary>How the add-in reads what the server answers.</summary>
    public sealed class ReadingTests
    {
        // A dimension as GET /dimensions/{name} might answer: every escape JSON has, numbers
        // written each way, fields the add-in does not know of every kind, and white space of
        // every kind or none between them. Written with ' for ", <e> for an e with an acute
        // accent as it is, and <t> for a tab.
        private const string Written = @"
{ 'name': 'Odd', 'type': 'standard', 'levels': ['Leaf', 'Top'],
  'later': { 'nested': { 'deep': [1, -2.5E-3, true, false, null, 'x\'y'] }, 'empty': {}, 'none': [] },
<t>'attributes': {'Short':'alias','Code':'string','Rank':'numeric'},
  'elements': [
    { 'name': 'Total \'All\'', 'type': 'consolidated', 'level': 1,
      'children': [
        { 'name': 'caf\u00e9', 'weight': 1 },
        { 'weight': -1.5e2, 'name': 'Tab\tand\\slash\/' },
        { 'name': '\ud83d\ude00 smile', 'weight': 0.1, 'later': [ {} ] }
      ],
      'attributes': { 'Short': 'Tot', 'Rank': 3 } },
    {'name':'caf<e>','type':'numeric','level':0,'parents':['Total \'All\''],'attributes':{'Code':'C\b\f\n\r','Rank':-1.5e2},'picklist':['a','b']},
    { 'name' : 'Tab\u0009and\u005Cslash/' , 'type' : 'numeric' , 'level' : 0 , 'parents' : [ 'Total \'All\'' ] }<t>,
    { 'name': '\uD83D\uDE00 smile', 'type': 'string', 'level': 0, 'parents': ['Total \'All\''] }
  ],
  'subsets': { 'Some': ['\ud83d\ude00 smile', 'caf\u00E9'], 'None': [] },
  'subsets_reading_cells': ['None'],
  'flag': true, 'nothing': null }
";

        internal static readonly string Text = Written.Replace('\'', '"').Replace("<e>", "\u00e9").Replace("<t>", "\t");

        private const string Total = "Total \"All\"";
        private const string Cafe = "caf\u00e9";
        private const string Tab = "Tab\tand\\slash/";
        private const string Smile = "\uD83D\uDE00 smile";

        /// <summary>Hands the text out a character at a time, as a slow network might: then
        /// every name, number and escape lies across two reads.</summary>
        private sealed class Trickle : TextReader
        {
            private readonly string text;
            private int at;

            public Trickle(string text)
            {
                this.text = text;
            }

            public override int Peek()
            {
                return at < text.Length ? text[at] : -1;
            }

            public override int Read()
            {
                return at < text.Length ? text[at++] : -1;
            }

            public override int Read(char[] buffer, int index, int count)
            {
                if (at == text.Length || count == 0)
                    return 0;
                buffer[index] = text[at++];
                return 1;
            }
        }

        [Theory]
        [InlineData(false)]
        [InlineData(true)]
        public void ADimensionIsReadHoweverItsTextComes(bool trickling)
        {
            var dimension = Dimension.Read(trickling ? new Trickle(Text) : (TextReader)new StringReader(Text));

            Assert.Equal(4.0, dimension.Size());
            Assert.Equal(2.0, dimension.Levels());
            // Each element is found by its name however it was written: escaped or not, and as
            // the name of a child, a parent or a member of a subset.
            Assert.Equal(new object[] { Total, Cafe, Tab, Smile }, Enumerable.Range(1, 4).Select(index => dimension.Name(index, "")));
            Assert.Equal(new object[] { 1.0, 2.0, 3.0, 4.0 }, new[] { Total, Cafe, Tab, Smile }.Select(dimension.Index));
            Assert.Equal(new object[] { Cafe, Tab, Smile }, Enumerable.Range(1, 3).Select(index => dimension.Child(Total, index)));
            Assert.Equal(new object[] { 1.0, -150.0, 0.1 }, new[] { Cafe, Tab, Smile }.Select(child => dimension.Weight(Total, child)));
            Assert.All(new[] { Cafe, Tab, Smile }, child => Assert.Equal(Total, dimension.Parent(child, 1)));
            Assert.Equal(0.0, dimension.ParentCount(Total));
            Assert.Equal(new object[] { "C", "N", "N", "S" }, new[] { Total, Cafe, Tab, Smile }.Select(dimension.Type));
            Assert.Equal(new object[] { 1.0, 0.0 }, new[] { Total, Smile }.Select(dimension.Level));

            Assert.Equal("Tot", dimension.Name(1, "Short"));
            Assert.Equal(1.0, dimension.Index("Tot"));
            Assert.Equal(Cafe, dimension.Attribute(Cafe, "Short"));
            Assert.Equal("C\b\f\n\r", dimension.Attribute(Cafe, "Code"));
            Assert.Equal(-150.0, dimension.AttributeNumber(Cafe, "Rank"));
            Assert.Equal("3", dimension.AttributeText(Total, "Rank"));

            Assert.Equal(2.0, dimension.SubsetSize("Some"));
            Assert.Equal(Smile, dimension.SubsetName("Some", 1.0, ""));
            Assert.Equal(Cafe, dimension.SubsetName("Some", "CAF\u00c9", ""));
            Assert.Equal("", dimension.SubsetName("Some", Total, ""));
            Assert.Equal(0.0, dimension.SubsetSize("None"));
        }

        [Fact]
        public void TextCutShortOrNotJsonIsRefused()
        {
            var whole = Text.TrimEnd();
            // Cut anywhere, whatever it was in the middle of.
            for (var length = 0; length < whole.Length; length++)
                Assert.Throws<FormatException>(() => Dimension.Read(new Trickle(whole.Substring(0, length))));
            foreach (var text in new[] { "[]", "{} {}", "{\"elements\": [1]}", "{\"elements\": [{\"name\": nope}]}", "{\"a\": 1,}" })
                Assert.Throws<FormatException>(() => Dimension.Read(new StringReader(text)));
        }

        [Fact]
        public void AWholeNumberStaysWholeToGoBackToTheServer()
        {
            var answer = Json.Read("{\"transaction\": 1234567890123450, \"value\": 3.0, \"weight\": -1.5e2, " +
                "\"huge\": 123456789012345678901234567890, \"list\": [-7, true, false, null, \"x\"], \"none\": {}}");
            // A transaction's id goes back in a path and in bodies: made a double, it went as
            // 1.23456789012345E+15.
            var transaction = Json.Field(answer, "transaction");
            Assert.Equal(1234567890123450L, transaction);
            Assert.Equal("transactions/1234567890123450", "transactions/" + transaction);
            Assert.Equal("{\"transaction\":1234567890123450}", Json.Write(new Dictionary<string, object> { ["transaction"] = transaction }));
            Assert.Equal(3.0, Json.Field(answer, "value"));
            Assert.Equal(-150.0, Json.Field(answer, "weight"));
            Assert.Equal(1.2345678901234568E+29, Json.Field(answer, "huge"));
            Assert.Equal(new object[] { -7L, true, false, null, "x" }, Json.Items(Json.Field(answer, "list")));
            Assert.Empty((Dictionary<string, object>)Json.Field(answer, "none"));
            Assert.Null(Json.Read(""));
            Assert.Throws<FormatException>(() => Json.Read("{} x"));
        }
    }
}
