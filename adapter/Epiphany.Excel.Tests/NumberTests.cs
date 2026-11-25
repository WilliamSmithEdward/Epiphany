using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using Xunit;

namespace Epiphany.Excel.Tests
{
    /// <summary>Numbers read and written exactly, as the server reads and writes them. The .NET
    /// Framework read about one in six hundred of the server's numerals as a neighbour, and its
    /// "R" format wrote about one number in four thousand as one; the expected bits here are
    /// those JavaScript, which reads numbers exactly, gives.</summary>
    public class NumberTests
    {
        [Theory]
        // Read as a neighbour by the .NET Framework.
        [InlineData("7331.400000000001", 0x40bca36666666667UL)]
        [InlineData("-7410.900000000001", 0xc0bcf2e666666667UL)]
        [InlineData("3.116323197525833e-156", 0x1fa56496f1316c6cUL)]
        [InlineData("2.285590587431993e-261", 0x09d1fe1d0bf059b1UL)]
        [InlineData("-3.993975721990293e-123", 0xa6851f36ee307866UL)]
        [InlineData("1892571.4285714286", 0x413ce0db6db6db6eUL)]
        [InlineData("-13745285.714285715", 0xc16a3790b6db6db7UL)]
        // Hard for any reader: half-way cases, and the edges of the normal and finite numbers.
        [InlineData("9007199254740993", 0x4340000000000000UL)]
        [InlineData("9007199254740995", 0x4340000000000002UL)]
        [InlineData("1e23", 0x44b52d02c7e14af6UL)]
        [InlineData("8.98846567431158e307", 0x7fe0000000000000UL)]
        [InlineData("2.2250738585072011e-308", 0x000fffffffffffffUL)]
        [InlineData("2.2250738585072012e-308", 0x0010000000000000UL)]
        [InlineData("4.9e-324", 0x0000000000000001UL)]
        [InlineData("2.4703282292062327e-324", 0x0000000000000000UL)]
        [InlineData("2.4703282292062328e-324", 0x0000000000000001UL)]
        [InlineData("1.7976931348623157e308", 0x7fefffffffffffffUL)]
        [InlineData("1.7976931348623159e308", 0x7ff0000000000000UL)]
        [InlineData("0.1", 0x3fb999999999999aUL)]
        [InlineData("-0", 0x8000000000000000UL)]
        [InlineData("123456789012345678901234567890", 0x45f8ee90ff6c373eUL)]
        [InlineData("0.000000000000000000000000000000000000000001", 0x37364cfda3281e39UL)]
        [InlineData("1.00000000000000011102230246251565404236316680908203125", 0x3ff0000000000000UL)]
        [InlineData("1.00000000000000011102230246251565404236316680908203124", 0x3ff0000000000000UL)]
        [InlineData("1.00000000000000011102230246251565404236316680908203126", 0x3ff0000000000001UL)]
        public void A_numeral_is_read_as_the_nearest_number(string numeral, ulong bits)
        {
            Assert.True(Numbers.TryRead(numeral, out var number));
            Assert.Equal(bits, (ulong)BitConverter.DoubleToInt64Bits(number));
            // A numeral as the server writes a number that is not whole is read so in answers.
            if (numeral.IndexOfAny(new[] { '.', 'e' }) >= 0)
            {
                var read = Convert.ToDouble(Json.Read(numeral), CultureInfo.InvariantCulture);
                Assert.Equal(bits, (ulong)BitConverter.DoubleToInt64Bits(read));
            }
        }

        [Theory]
        [InlineData("")]
        [InlineData("-")]
        [InlineData(".")]
        [InlineData("1e")]
        [InlineData("1e+")]
        [InlineData("1.2.3")]
        [InlineData("12a")]
        [InlineData("NaN")]
        public void What_is_no_numeral_is_not_read(string text)
        {
            Assert.False(Numbers.TryRead(text, out _));
        }

        [Theory]
        // Written by "R" with fifteen digits that name a neighbour.
        [InlineData(0x31565c5754b64e8eUL)]
        [InlineData(0x8004705f7a479a4bUL)]
        [InlineData(0xa31b0cb38d64ca0eUL)]
        [InlineData(0xdd205408f6fbe18dUL)]
        [InlineData(0x0225856e7b65c3f4UL)]
        public void A_number_is_written_as_text_that_names_it(ulong bits)
        {
            var number = BitConverter.Int64BitsToDouble((long)bits);
            Assert.True(Numbers.TryRead(Numbers.Text(number), out var back));
            Assert.Equal(bits, (ulong)BitConverter.DoubleToInt64Bits(back));
            var answered = Convert.ToDouble(Json.Read(Json.Write(number)), CultureInfo.InvariantCulture);
            Assert.Equal(bits, (ulong)BitConverter.DoubleToInt64Bits(answered));
        }

        [Fact]
        public void A_number_is_written_as_short_as_it_names_itself()
        {
            Assert.Equal(new[] { "0.1", "7331.4", "2024", "-0.5", "1E-07" },
                new[] { 0.1, 7331.4, 2024, -0.5, 1e-7 }.Select(Numbers.Text));
        }

        [Fact]
        public void Every_number_goes_out_and_comes_back_as_itself()
        {
            var random = new Random(7);
            var bytes = new byte[8];
            for (var tried = 0; tried < 200_000; tried++)
            {
                random.NextBytes(bytes);
                var number = (tried % 3) switch
                {
                    0 => BitConverter.ToDouble(bytes, 0),
                    1 => random.Next(-1_000_000, 1_000_000) / 7.0 * Math.Pow(10, random.Next(-4, 9)),
                    _ => random.Next() / 100.0,
                };
                if (double.IsNaN(number) || double.IsInfinity(number))
                    continue;
                var bits = BitConverter.DoubleToInt64Bits(number);
                foreach (var text in new[] { Numbers.Text(number), number.ToString("G17", CultureInfo.InvariantCulture) })
                {
                    Assert.True(Numbers.TryRead(text, out var back), text);
                    Assert.True(bits == BitConverter.DoubleToInt64Bits(back), $"{text} read as {back:G17}");
                }
            }
        }

        [Fact]
        public void Json_is_written_as_the_server_reads_it()
        {
            var body = new Dictionary<string, object>
            {
                ["cube"] = "Sales",
                ["at"] = new[] { "Jan", "US" },
                ["value"] = 0.1,
                ["transaction"] = 1234567890123456789L,
                ["held"] = true,
                ["none"] = null,
                ["cells"] = new object[] { new Dictionary<string, object> { ["value"] = -2.5 } },
            };
            Assert.Equal(
                "{\"cube\":\"Sales\",\"at\":[\"Jan\",\"US\"],\"value\":0.1,\"transaction\":1234567890123456789,"
                    + "\"held\":true,\"none\":null,\"cells\":[{\"value\":-2.5}]}",
                Json.Write(body));
        }

        [Fact]
        public void Text_with_any_character_comes_back_as_it_went()
        {
            var every = new string(Enumerable.Range(0, 0x500).Select(code => (char)code).ToArray())
                + "a quote \" and a backslash \\ " + char.ConvertFromUtf32(0x1F600);
            var json = Json.Write(new Dictionary<string, object> { ["text"] = every });
            Assert.DoesNotContain(json, c => c < ' ');
            Assert.Equal(every, Json.Field(Json.Read(json), "text"));
        }
    }
}
