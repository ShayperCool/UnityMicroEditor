using System;
using System.Collections;
using System.Reflection;
using UnityEngine;
using UnityEngine.Scripting;

namespace Dungeon.CLIPlay
{
    // End-of-frame coroutines remain in the ungated render stages while the
    // agent pauses game Update/FixedUpdate. The object is owned by the bridge.
    [Preserve]
    public sealed class AgentCaptureRunner : MonoBehaviour
    {
        internal static MethodInfo ScreenCaptureMethod()
        {
            Type type = Type.GetType("UnityEngine.ScreenCapture, UnityEngine.ScreenCaptureModule", false);
            if (type == null) return null;
            MethodInfo method = type.GetMethod("CaptureScreenshotAsTexture", BindingFlags.Public | BindingFlags.Static,
                null, new[] { typeof(int) }, null);
            return method != null && method.ReturnType == typeof(Texture2D) ? method : null;
        }

        internal void Capture(Action<Texture2D> success, Action<Exception> failure)
        { StartCoroutine(AtEndOfFrame(success, failure)); }

        private static IEnumerator AtEndOfFrame(Action<Texture2D> success, Action<Exception> failure)
        {
            yield return new WaitForEndOfFrame();
            Texture2D texture = null;
            try
            {
                MethodInfo method = ScreenCaptureMethod();
                if (method == null) throw new InvalidOperationException("This host profile has no public ScreenCapture module API.");
                texture = method.Invoke(null, new object[] { 1 }) as Texture2D;
                if (texture == null) throw new InvalidOperationException("The renderer returned no screen texture.");
                success(texture);
            }
            catch (Exception error) { failure(error.InnerException ?? error); }
            finally { if (texture != null) UnityEngine.Object.Destroy(texture); }
        }
    }
}
