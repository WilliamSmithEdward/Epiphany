using System;
using System.Diagnostics;
using System.IO;

namespace Epiphany.Excel.Tests
{
    /// <summary>A real server on a copy of the example model, stopped and removed when disposed.
    /// The adapter holds no OLAP logic of its own, so its tests ask the server itself.</summary>
    public class Home : IDisposable
    {
        // Made up for these tests; they open nothing else.
        public const string User = "ada";
        public const string Password = "test-only-5d81e0";

        private const string Line = @"{
  ""name"": ""Line"",
  ""type"": ""standard"",
  ""elements"": [
    { ""name"": ""Comment"", ""type"": ""string"" },
    { ""name"": ""Amount"", ""type"": ""numeric"", ""attributes"": { ""Short"": ""Amt"" } }
  ],
  ""attributes"": { ""Short"": ""alias"" },
  ""subsets"": {
    ""Texts"": { ""elements"": [""Comment""] },
    ""Backwards"": { ""elements"": [""Amount"", ""Comment""] }
  }
}";
        private const string Notes = @"{ ""name"": ""Notes"", ""dimensions"": [""Entity"", ""Line""] }";

        private readonly string directory;
        private readonly Process server;

        public string Address { get; }

        public Home() : this(false) { }

        protected Home(bool withUser)
        {
            var root = AppContext.BaseDirectory;
            while (!File.Exists(Path.Combine(root, "Cargo.toml")))
                root = Path.GetDirectoryName(root) ?? throw new InvalidOperationException("not inside the repository");
            var binary = Environment.GetEnvironmentVariable("EPIPHANY_SERVER")
                ?? Path.Combine(root, "target", "debug", "epiphany-server.exe");
            if (!File.Exists(binary))
                throw new InvalidOperationException(
                    binary + " is missing; run 'cargo build -p epiphany-server' first");

            directory = Path.Combine(Path.GetTempPath(), "epiphany-adapter-" + Guid.NewGuid().ToString("N"));
            Copy(Path.Combine(root, "crates", "epiphany-core", "tests", "fixtures", "model"),
                Path.Combine(directory, "model"));
            File.WriteAllText(Path.Combine(directory, "model", "dimensions", "line.json"), Line);
            File.WriteAllText(Path.Combine(directory, "model", "cubes", "notes.json"), Notes);
            if (withUser)
            {
                var adding = Start(binary, "user \"" + directory + "\" " + User);
                adding.StandardInput.WriteLine(Password);
                adding.StandardInput.Close();
                adding.WaitForExit();
                if (adding.ExitCode != 0)
                    throw new InvalidOperationException("the server could not add a user");
            }

            // Port zero lets the system choose a free port; the server says which it got.
            server = Start(binary, "serve \"" + directory + "\" 127.0.0.1:0");
            var said = server.StandardOutput.ReadLine() ?? "";
            var from = said.IndexOf("http://", StringComparison.Ordinal);
            if (from < 0)
                throw new InvalidOperationException("the server did not start: " + said);
            Address = said.Substring(from).Split(' ')[0];
        }

        private static Process Start(string binary, string arguments)
        {
            return Process.Start(new ProcessStartInfo(binary, arguments)
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardInput = true,
                RedirectStandardOutput = true,
            });
        }

        private static void Copy(string from, string to)
        {
            Directory.CreateDirectory(to);
            foreach (var file in Directory.GetFiles(from))
                File.Copy(file, Path.Combine(to, Path.GetFileName(file)));
            foreach (var child in Directory.GetDirectories(from))
                Copy(child, Path.Combine(to, Path.GetFileName(child)));
        }

        public void Dispose()
        {
            try
            {
                server.Kill();
                server.WaitForExit();
                Directory.Delete(directory, true);
            }
            catch (Exception)
            {
                // Nothing a test can do about a server that is already gone.
            }
        }
    }

    /// <summary>The same, with one user, so that the server asks for a sign-in.</summary>
    public sealed class SecuredHome : Home
    {
        public SecuredHome() : base(true) { }
    }
}
