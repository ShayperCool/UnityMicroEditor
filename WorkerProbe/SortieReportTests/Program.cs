using System;
using System.Reflection;
using System.Text.Json;
using Dungeon.Net;
using UnityPlay.MultiplayerProof;

internal static class Program
{
    private static void Check(bool condition, string message) { if (!condition) throw new Exception(message); }
    private static int Main()
    {
        Type contract = typeof(MultiplayerObserver.SortieSnapshot);
        Check(Attribute.IsDefined(contract, typeof(SerializableAttribute)), "Report DTO must carry JsonUtility's Serializable marker");
        var fields = contract.GetFields(BindingFlags.Instance | BindingFlags.Public);
        Check(fields.Length == 6, "Every network sortie field must be represented");
        foreach (FieldInfo field in fields) Check(field.FieldType == typeof(int), "Report fields must preserve signed values and byte widening as JSON numbers");
        var original = new SortieState { archetype = 7, size = 255, shipSeed = int.MinValue, scenarioSeed = int.MaxValue, crew = 255, generation = -1 };
        var dto = MultiplayerObserver.SortieSnapshot.From(original);
        Check(dto.archetype == original.archetype && dto.size == original.size && dto.shipSeed == original.shipSeed &&
            dto.scenarioSeed == original.scenarioSeed && dto.crew == original.crew && dto.generation == original.generation, "Live network values must be copied without substitution or narrowing");
        string json = JsonSerializer.Serialize(dto, new JsonSerializerOptions { IncludeFields = true });
        using JsonDocument encoded = JsonDocument.Parse(json);
        foreach (string name in new[] { "archetype", "size", "shipSeed", "scenarioSeed", "crew", "generation" })
            Check(encoded.RootElement.GetProperty(name).ValueKind == JsonValueKind.Number, "Driver JSON field missing: " + name);
        Check(encoded.RootElement.GetProperty("shipSeed").GetInt32() == int.MinValue && encoded.RootElement.GetProperty("crew").GetInt32() == 255, "Signed seed and byte bounds must survive JSON");
        Console.WriteLine("Serializable sortie report SDK regression passed; native JsonUtility is checked by the actual Player driver.");
        return 0;
    }
}
