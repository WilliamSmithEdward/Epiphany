using System;
using System.Globalization;
using System.Numerics;

namespace Epiphany.Excel
{
    /// <summary>Numbers read from text and written as text exactly, as the server reads and
    /// writes them. The .NET Framework, which Excel loads the add-in into, does neither: its
    /// parsing read about one numeral in six hundred of those the server writes as a
    /// neighbour (7331.400000000001 as 7331.4000000000015), and "R" wrote about one number in
    /// four thousand as fifteen digits that name a neighbour (5.06228479185094E-71). .NET Core
    /// 3.0 mended both.</summary>
    internal static class Numbers
    {
        /// <summary>The powers of ten a double holds exactly.</summary>
        private static readonly double[] Powers =
        {
            1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
            1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
        };

        /// <summary>The number a numeral names, to the nearest, half-way cases going to the even
        /// one: a sign or none, digits with a point among them or not, and an exponent or none.
        /// False for anything else.</summary>
        public static bool TryRead(string numeral, out double number)
        {
            number = 0;
            var at = 0;
            var negative = at < numeral.Length && numeral[at] == '-';
            if (at < numeral.Length && (numeral[at] == '-' || numeral[at] == '+'))
                at++;
            // The first nineteen significant digits as a whole number, how many there are in
            // all, and how many stand after the point.
            ulong head = 0;
            int significant = 0, afterPoint = 0;
            bool point = false, any = false;
            var start = at;
            for (; at < numeral.Length; at++)
            {
                var c = numeral[at];
                if (c == '.' && !point)
                {
                    point = true;
                    continue;
                }
                if (c < '0' || c > '9')
                    break;
                any = true;
                if (point)
                    afterPoint++;
                if (significant == 0 && c == '0')
                    continue;
                if (significant < 19)
                    head = head * 10 + (ulong)(c - '0');
                significant++;
            }
            if (!any)
                return false;
            var end = at;
            var power = 0;
            if (at < numeral.Length && (numeral[at] == 'e' || numeral[at] == 'E'))
            {
                at++;
                var minus = at < numeral.Length && numeral[at] == '-';
                if (at < numeral.Length && (numeral[at] == '-' || numeral[at] == '+'))
                    at++;
                var digits = at;
                for (; at < numeral.Length && numeral[at] >= '0' && numeral[at] <= '9'; at++)
                    power = Math.Min(power * 10 + (numeral[at] - '0'), 1_000_000);
                if (at == digits)
                    return false;
                if (minus)
                    power = -power;
            }
            if (at != numeral.Length)
                return false;
            // The number is the digits, as a whole number, times ten to this.
            var exponent = power - afterPoint;
            double magnitude;
            if (significant == 0)
                magnitude = 0;
            // Fifteen digits are exactly a double, and so is ten to at most twenty-two: one
            // multiplication or division by it is rounded once, to the nearest.
            else if (significant <= 15 && exponent >= -22 && exponent <= 22)
                magnitude = exponent < 0 ? head / Powers[-exponent] : head * Powers[exponent];
            else
                magnitude = Exact(numeral, start, end, head, significant, exponent);
            number = negative ? -magnitude : magnitude;
            return true;
        }

        /// <summary>A number as text that names it exactly: as short as "R" writes it where that
        /// reads back as the number, and otherwise with all seventeen digits.</summary>
        public static string Text(double number)
        {
            var text = number.ToString("R", CultureInfo.InvariantCulture);
            var exact = !TryRead(text, out var back)
                || BitConverter.DoubleToInt64Bits(back) == BitConverter.DoubleToInt64Bits(number);
            return exact ? text : number.ToString("G17", CultureInfo.InvariantCulture);
        }

        /// <summary>Ten to each power a numeral of up to nineteen digits can need, made once,
        /// with how many bits each takes.</summary>
        private static readonly BigInteger[] Tens = new BigInteger[350];
        private static readonly int[] TensBits = new int[350];

        static Numbers()
        {
            Tens[0] = BigInteger.One;
            for (var power = 1; power < Tens.Length; power++)
                Tens[power] = Tens[power - 1] * 10;
            for (var power = 0; power < Tens.Length; power++)
                TensBits[power] = Bits(Tens[power]);
        }

        /// <summary>The nearest number to the digits of a numeral times ten to an exponent,
        /// worked out with whole numbers as large as they need to be.</summary>
        private static double Exact(string numeral, int start, int end, ulong head, int significant, int exponent)
        {
            // Beyond the largest number, or below half the smallest, whatever the digits.
            if (significant + exponent > 310)
                return double.PositiveInfinity;
            if (significant + exponent < -330)
                return 0;
            var whole = significant <= 19 ? new BigInteger(head) : Digits(numeral, start, end);
            var wholeBits = significant <= 19 ? Width(head) : Bits(whole);
            if (exponent >= 0)
            {
                // Sixty-four bits of the product are enough to round, with whether any bit
                // below them is one.
                var product = whole * Ten(exponent);
                var excess = Math.Max(0, Bits(product) - 64);
                var kept = product >> excess;
                return Nearest((ulong)kept, product != kept << excess, excess);
            }
            // The quotient is taken with two bits at least below the nearest number's last bit,
            // however small the number, which leaves it at most 57 bits.
            var divisor = Ten(-exponent);
            var divisorBits = -exponent < Tens.Length ? TensBits[-exponent] : Bits(divisor);
            var lowest = Math.Max(wholeBits - divisorBits - 53, -1074);
            var shift = Math.Max(0, 2 - lowest);
            var quotient = BigInteger.DivRem(whole << shift, divisor, out var remainder);
            return Nearest((ulong)quotient, !remainder.IsZero, -shift);
        }

        /// <summary>The nearest number to a whole number times two to a power, and something
        /// less than one more where <paramref name="beyond"/> says so; half-way cases go to the
        /// even one.</summary>
        private static double Nearest(ulong whole, bool beyond, int power)
        {
            if (whole == 0)
                return 0;
            var lowest = Math.Max(Width(whole) - 1 + power - 52, -1074);
            var dropped = lowest - power;
            ulong bits;
            if (dropped <= 0)
                bits = whole << -dropped;
            else
            {
                bits = whole >> dropped;
                var rest = whole & ((1UL << dropped) - 1);
                var half = 1UL << (dropped - 1);
                if (rest > half || (rest == half && (beyond || (bits & 1) == 1)))
                    bits++;
            }
            // Rounded up to 2^53, it is 2^52 of the next power of two.
            if (bits == 1UL << 53)
            {
                bits >>= 1;
                lowest++;
            }
            // Below 2^-1022 the mantissa is the number's bits as they are.
            if (bits < 1UL << 52)
                return BitConverter.Int64BitsToDouble((long)bits);
            var biased = lowest + 1075;
            return biased >= 2047
                ? double.PositiveInfinity
                : BitConverter.Int64BitsToDouble((long)biased << 52 | (long)(bits - (1UL << 52)));
        }

        private static BigInteger Ten(int power)
        {
            return power < Tens.Length ? Tens[power] : BigInteger.Pow(10, power);
        }

        /// <summary>The digits of a numeral as a whole number, its point passed over.</summary>
        private static BigInteger Digits(string numeral, int start, int end)
        {
            var whole = BigInteger.Zero;
            for (var at = start; at < end; at++)
            {
                if (numeral[at] != '.')
                    whole = whole * 10 + (numeral[at] - '0');
            }
            return whole;
        }

        /// <summary>How many bits a whole number above nought takes.</summary>
        private static int Width(ulong whole)
        {
            var width = 0;
            for (; whole != 0; whole >>= 1)
                width++;
            return width;
        }

        private static int Bits(BigInteger whole)
        {
            var bytes = whole.ToByteArray();
            var last = bytes.Length - 1;
            while (last > 0 && bytes[last] == 0)
                last--;
            var bits = last * 8;
            for (int top = bytes[last]; top > 0; top >>= 1)
                bits++;
            return bits;
        }
    }
}
