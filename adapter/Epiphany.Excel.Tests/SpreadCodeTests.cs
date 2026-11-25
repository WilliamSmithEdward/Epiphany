using Xunit;

namespace Epiphany.Excel.Tests
{
    public sealed class SpreadCodeTests
    {
        [Theory]
        [InlineData("P1200", "proportional", 1200.0, 0)]
        [InlineData(" s 50 ", "equal", 50.0, 0)]
        [InlineData("LS50", "equal", 50.0, 0)]
        [InlineData("r7", "repeat", 7.0, 0)]
        [InlineData("LR7", "repeat", 7.0, 0)]
        [InlineData("C", "repeat", 0.0, 0)]
        [InlineData("P+100", "proportional", 100.0, 1)]
        [InlineData("S~2.5", "equal", 2.5, -1)]
        [InlineData("p -40", "proportional", -40.0, 0)]
        [InlineData("P%10", "proportional", 10.0, 0)]
        [InlineData("p% -25", "proportional", -25.0, 0)]
        public void TheCodesOfTheGridAreTaken(string typed, string method, double value, int change)
        {
            var spread = SpreadCode.Parse(typed);
            Assert.NotNull(spread);
            Assert.Equal((method, value, change), (spread.Method, spread.Value, spread.Change));
        }

        [Theory]
        [InlineData("Revenue")]
        [InlineData("1200")]
        [InlineData("C5")]
        [InlineData("C+")]
        [InlineData("R+10")]
        [InlineData("P")]
        [InlineData("P%")]
        [InlineData("P%+10")]
        [InlineData("")]
        public void AnythingElseIsNoSpread(string typed)
        {
            Assert.Null(SpreadCode.Parse(typed));
        }

        [Fact]
        public void NumbersAreNoSpread()
        {
            Assert.Null(SpreadCode.Parse(1200.0));
        }

        [Fact]
        public void AChangeIsMadeToWhatTheCellHolds()
        {
            Assert.Equal(1300.0, SpreadCode.Parse("P+100").ComesTo(1200.0));
            Assert.Equal(1100.0, SpreadCode.Parse("S~100").ComesTo(1200.0));
            Assert.Equal(500.0, SpreadCode.Parse("P500").ComesTo(1200.0));
            // A percentage changes what the cell holds, up or down.
            Assert.Equal(1320.0, SpreadCode.Parse("P%10").ComesTo(1200.0), 9);
            Assert.Equal(900.0, SpreadCode.Parse("P%-25").ComesTo(1200.0), 9);
            // Only a change or a percentage asks what the cell holds first.
            Assert.True(SpreadCode.Parse("P%10").Relative);
            Assert.True(SpreadCode.Parse("S~100").Relative);
            Assert.False(SpreadCode.Parse("P500").Relative);
        }
    }
}
