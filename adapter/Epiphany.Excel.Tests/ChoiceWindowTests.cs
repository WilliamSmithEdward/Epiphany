using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Runtime.ExceptionServices;
using System.Threading;
using System.Windows.Forms;
using Xunit;

namespace Epiphany.Excel.Tests
{
    /// <summary>The window that chooses the element of a SUBNM, made but not shown.</summary>
    public sealed class ChoiceWindowTests
    {
        private static List<KeyValuePair<string, string>> Members(params string[] pairs)
        {
            return Enumerable.Range(0, pairs.Length / 2)
                .Select(at => new KeyValuePair<string, string>(pairs[2 * at], pairs[2 * at + 1]))
                .ToList();
        }

        // Windows are made on a thread of the kind Excel's own is.
        private static void OnWindowThread(Action test)
        {
            ExceptionDispatchInfo failed = null;
            var thread = new Thread(() =>
            {
                try
                {
                    test();
                }
                catch (Exception error)
                {
                    failed = ExceptionDispatchInfo.Capture(error);
                }
            });
            thread.SetApartmentState(ApartmentState.STA);
            thread.Start();
            thread.Join();
            failed?.Throw();
        }

        [Fact]
        public void TypingNarrowsTheListAndTheKeysMoveTheChoice()
        {
            var months = Members("Jan", "January", "Feb", "February", "Mar", "March", "Apr", "April");
            OnWindowThread(() =>
            {
                using (var window = new ChoiceWindow(months, "February"))
                {
                    // What the cell shows is chosen to begin with, and gives the element's name.
                    Assert.Equal("Feb", window.Chosen);
                    Assert.Equal("4 elements", window.Counted);

                    window.Typed = "UARY";
                    Assert.Equal(2, window.Listed);
                    Assert.Equal("2 of 4", window.Counted);
                    Assert.Equal("Feb", window.Chosen);
                    Assert.True(window.Press(Keys.Up));
                    Assert.Equal("Jan", window.Chosen);
                    Assert.True(window.Press(Keys.Up));
                    Assert.Equal("Jan", window.Chosen);
                    Assert.True(window.Press(Keys.PageDown));
                    Assert.Equal("Feb", window.Chosen);
                    Assert.False(window.Press(Keys.A));

                    window.Typed = "zz";
                    Assert.Equal(0, window.Listed);
                    Assert.Null(window.Chosen);
                    Assert.False(window.Press(Keys.A));
                    Assert.True(window.Press(Keys.Down));
                    Assert.Null(window.Chosen);

                    window.Typed = "";
                    Assert.Equal("Feb", window.Chosen);
                }
            });
        }

        [Fact]
        public void AMillionElementsAreListedAndNarrowed()
        {
            var many = Enumerable.Range(1, 1000000)
                .Select(index => new KeyValuePair<string, string>("l" + index, "Leaf " + index))
                .ToList();
            OnWindowThread(() =>
            {
                using (var window = new ChoiceWindow(many, "Leaf 500000"))
                {
                    Assert.Equal("l500000", window.Chosen);
                    Assert.Equal(1000000.ToString("N0", CultureInfo.CurrentCulture) + " elements", window.Counted);
                    // Leaf 99999, and Leaf 999990 to Leaf 999999.
                    window.Typed = "leaf 99999";
                    Assert.Equal(11, window.Listed);
                    Assert.Equal("l99999", window.Chosen);
                    Assert.True(window.Press(Keys.PageDown));
                    Assert.Equal("l999999", window.Chosen);
                }
            });
        }
    }
}
