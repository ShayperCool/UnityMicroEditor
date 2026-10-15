// Stands in for the applier in the "faulty" runs of the harness. The bridge has to answer for an
// entry point that throws or returns no ack, and to publish unchanged what a working one returns.
using System;
using System.IO;

namespace UnityMicroEditor.HotReload
{
    public static class Entry
    {
        public static string Handle(string requestPath)
        {
            string request = File.ReadAllText(requestPath);
            if (request.Contains("\"generation\":1,")) throw new InvalidOperationException("The faulty applier threw.");
            if (request.Contains("\"generation\":2,")) return "no ack";
            return "{\"appliedGeneration\":7,\"published\":\"as it is\"}\n";
        }
    }
}
