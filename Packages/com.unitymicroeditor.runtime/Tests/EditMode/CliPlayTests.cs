using System;
using System.IO;
using NUnit.Framework;
using UnityEngine.LowLevel;

namespace Dungeon.CLIPlay.Tests
{
    public sealed class CliPlayTests
    {
        private sealed class Marker { }
        private sealed class ExistingSystem { }

        [Test]
        public void PlayerWithoutReportArgumentHasNoObserver()
        {
            Assert.That(CliPlayArguments.TryParse(new[] { "game", "-batchmode", "--unity-cli-frames", "bad" }, out var options), Is.False);
            Assert.That(options, Is.Null);
        }

        [Test]
        public void ParsesObserverAndPreservesUnityArguments()
        {
            string path = Path.Combine(Path.GetTempPath(), "cli-player-test.json");
            Assert.That(CliPlayArguments.TryParse(new[]
            {
                "game", "-batchmode", "-nographics", "--unity-cli-report", path,
                "--unity-cli-frames", "120", "--unity-cli-scene", "1",
            }, out var options), Is.True);
            Assert.That(options.ReportPath, Is.EqualTo(path));
            Assert.That(options.Frames, Is.EqualTo(120));
            Assert.That(options.Scene, Is.EqualTo("1"));
            Assert.That(options.ReadyPath, Is.EqualTo(path + ".ready.json"));
            Assert.That(options.TargetFrameRate, Is.Null);
            Assert.That(options.NoVsync, Is.False);
        }

        [TestCase("-1", -1)]
        [TestCase("120", 120)]
        public void ParsesBenchmarkControlsAndSeparateArtifacts(string frameRate, int expectedRate)
        {
            string root = Path.GetTempPath();
            Assert.That(CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", Path.Combine(root, "runtime.json"), "--unity-cli-frames", "10",
                "--unity-cli-ready", Path.Combine(root, "ready.json"), "--unity-cli-screenshot", Path.Combine(root, "frame.png"),
                "--unity-cli-content", Path.Combine(root, "current.json"), "--unity-cli-fps", frameRate, "--unity-cli-no-vsync",
            }, out var options), Is.True);
            Assert.That(options.TargetFrameRate, Is.EqualTo(expectedRate));
            Assert.That(options.NoVsync, Is.True);
            Assert.That(options.ReadyPath, Is.EqualTo(Path.Combine(root, "ready.json")));
            Assert.That(options.ScreenshotPath, Is.EqualTo(Path.Combine(root, "frame.png")));
            Assert.That(options.ContentManifestPath, Is.EqualTo(Path.Combine(root, "current.json")));
        }

        [TestCase("0")]
        [TestCase("-2")]
        [TestCase("3.5")]
        [TestCase("2147483648")]
        public void RejectsInvalidTargetFrameRates(string rate)
        {
            string path = Path.Combine(Path.GetTempPath(), "runtime.json");
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-fps", rate,
            }, out _));
        }

        [Test]
        public void RejectsArtifactCollisionsRelativePathsAndDuplicateBenchmarkFlags()
        {
            string path = Path.Combine(Path.GetTempPath(), "runtime.json");
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-ready", path,
            }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-content", path,
            }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-screenshot", "relative.png",
            }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-fps", "60", "--unity-cli-fps", "120",
            }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-no-vsync", "--unity-cli-no-vsync",
            }, out _));
        }

        [Test]
        public void InteractiveObserverAcceptsContentAndRejectsDuplicateFlagOrShutdownCapture()
        {
            string root = Path.GetTempPath();
            string report = Path.Combine(root, "runtime.json");
            Assert.That(CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", report, "--unity-cli-frames", int.MaxValue.ToString(),
                "--unity-cli-interactive", "--unity-cli-content", Path.Combine(root, "current.json"),
            }, out var options), Is.True);
            Assert.That(options.Interactive, Is.True);
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", report, "--unity-cli-frames", "10", "--unity-cli-interactive", "--unity-cli-interactive",
            }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", report, "--unity-cli-frames", "10", "--unity-cli-interactive",
                "--unity-cli-screenshot", Path.Combine(root, "frame.png"),
            }, out _));
        }

        [TestCase("0")]
        [TestCase("-1")]
        [TestCase("1.5")]
        [TestCase("2147483648")]
        public void RejectsInvalidFrameBudgets(string budget)
        {
            string path = Path.Combine(Path.GetTempPath(), "cli-player-test.json");
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", budget,
            }, out _));
        }

        [Test]
        public void RejectsMissingAndDuplicateArguments()
        {
            string path = Path.Combine(Path.GetTempPath(), "cli-player-test.json");
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[] { "--unity-cli-report", path }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[] { "--unity-cli-report", "--unity-cli-frames", "10" }, out _));
            Assert.Throws<FormatException>(() => CliPlayArguments.TryParse(new[]
            {
                "--unity-cli-report", path, "--unity-cli-frames", "10", "--unity-cli-frames", "20",
            }, out _));
        }

        [Test]
        public void FrameObserverRunsAfterExistingPostLateUpdateSystemsAndRemovalRestoresThem()
        {
            int existingCalls = 0;
            int observerCalls = 0;
            var existing = new PlayerLoopSystem { type = typeof(ExistingSystem), updateDelegate = () => existingCalls++ };
            var loop = new PlayerLoopSystem
            {
                subSystemList = new[]
                {
                    new PlayerLoopSystem { type = typeof(UnityEngine.PlayerLoop.Update) },
                    new PlayerLoopSystem { type = typeof(UnityEngine.PlayerLoop.PostLateUpdate), subSystemList = new[] { existing } },
                },
            };
            Assert.That(FrameLoop.AppendEndOfFrame(ref loop, typeof(Marker), () => observerCalls++), Is.True);
            Assert.That(loop.subSystemList[0].type, Is.EqualTo(typeof(UnityEngine.PlayerLoop.Update)));
            var systems = loop.subSystemList[1].subSystemList;
            Assert.That(systems.Length, Is.EqualTo(2));
            Assert.That(systems[0].type, Is.EqualTo(typeof(ExistingSystem)));
            Assert.That(systems[1].type, Is.EqualTo(typeof(Marker)));
            systems[0].updateDelegate();
            systems[1].updateDelegate();
            Assert.That(existingCalls, Is.EqualTo(1));
            Assert.That(observerCalls, Is.EqualTo(1));

            Assert.That(FrameLoop.Remove(ref loop, typeof(Marker)), Is.True);
            systems = loop.subSystemList[1].subSystemList;
            Assert.That(systems.Length, Is.EqualTo(1));
            Assert.That(systems[0].updateDelegate, Is.EqualTo(existing.updateDelegate));
            Assert.That(FrameLoop.Remove(ref loop, typeof(Marker)), Is.False);
        }

        [Test]
        public void MissingPostLateUpdateDoesNotReplaceThePlayerLoop()
        {
            var systems = new[] { new PlayerLoopSystem { type = typeof(UnityEngine.PlayerLoop.Update) } };
            var loop = new PlayerLoopSystem { subSystemList = systems };
            Assert.That(FrameLoop.AppendEndOfFrame(ref loop, typeof(Marker), () => { }), Is.False);
            Assert.That(loop.subSystemList, Is.SameAs(systems));
        }
    }
}
