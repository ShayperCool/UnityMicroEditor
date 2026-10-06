import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from host import stage_player
from unity_play import ToolError


class NativePlayerStagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.host = self.root / 'host/Host.x86_64'
        self.data = self.host.parent / 'Host_Data'
        (self.data / 'Managed').mkdir(parents=True)
        self.host.write_bytes(b'native executable')
        (self.host.parent / 'UnityPlayer.so').write_bytes(b'native engine')
        (self.host.parent / 'host.json').write_text('{}')
        (self.data / 'Managed/Engine.dll').write_bytes(b'engine sdk')
        (self.data / 'ScriptingAssemblies.json').write_text('{"names":["Engine.dll"],"types":[1]}')
        source = self.root / 'code/Game.dll'
        source.parent.mkdir()
        source.write_bytes(b'verified source DLL')
        source.with_suffix('.pdb').write_bytes(b'source symbols')
        self.compiler = {'assemblies': [{'name': 'Game', 'path': str(source), 'sha256': hashlib.sha256(source.read_bytes()).hexdigest()}]}
        self.revision = self.root / 'revision'
        self.revision.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def test_registers_source_at_native_startup_without_mutating_reusable_host(self):
        original = (self.data / 'ScriptingAssemblies.json').read_bytes()
        staged = stage_player(self.host, self.revision, self.compiler)
        player = Path(staged['player'])
        table = json.loads((player.parent / 'Host_Data/ScriptingAssemblies.json').read_text())
        self.assertEqual(table, {'names': ['Engine.dll', 'Game.dll'], 'types': [1, 16]})
        self.assertEqual(Path(staged['assemblies'][0]).read_bytes(), b'verified source DLL')
        self.assertEqual(Path(staged['assemblies'][0]).with_suffix('.pdb').read_bytes(), b'source symbols')
        self.assertEqual((self.data / 'ScriptingAssemblies.json').read_bytes(), original)
        self.assertFalse((self.data / 'Managed/Game.dll').exists())
        self.assertEqual((player.parent / 'UnityPlayer.so').read_bytes(), b'native engine')
        self.assertFalse(staged['hostModified'])

    def test_rejects_changed_source_dll_before_registration(self):
        Path(self.compiler['assemblies'][0]['path']).write_bytes(b'changed after compilation')
        with self.assertRaisesRegex(ToolError, 'changed before'):
            stage_player(self.host, self.revision, self.compiler)
        self.assertFalse((self.revision / 'Player').exists())

    def test_rejects_source_name_collision_and_malformed_engine_table(self):
        self.compiler['assemblies'][0]['name'] = 'Engine'
        self.compiler['assemblies'][0]['path'] = str(self.data / 'Managed/Engine.dll')
        with self.assertRaisesRegex(ToolError, 'conflicts'):
            stage_player(self.host, self.revision, self.compiler)
        (self.data / 'ScriptingAssemblies.json').write_text('{"names":[{}],"types":[1]}')
        with self.assertRaisesRegex(ToolError, 'schema'):
            stage_player(self.host, self.revision, self.compiler)
