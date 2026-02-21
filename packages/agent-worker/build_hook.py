"""Custom hatch build hook to compile proto files during build."""

import subprocess
import sys
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface


class CustomBuildHook(BuildHookInterface):
    def initialize(self, version, build_data):
        """Compile proto files before building the package."""
        proto_dir = Path(self.root) / ".." / ".." / "proto"
        output_dir = Path(self.root) / "src" / "agent_worker"

        proto_file = proto_dir / "agent.proto"
        if not proto_file.exists():
            # Skip proto compilation in isolated builds (e.g. uv sync).
            # CI setup-command pre-compiles stubs into src/agent_worker/.
            return

        subprocess.check_call(
            [
                sys.executable,
                "-m",
                "grpc_tools.protoc",
                f"-I{proto_dir}",
                f"--python_out={output_dir}",
                f"--grpc_python_out={output_dir}",
                str(proto_file),
            ]
        )

        # Fix grpc_tools generating bare imports instead of relative imports
        grpc_stub = output_dir / "agent_pb2_grpc.py"
        if grpc_stub.exists():
            content = grpc_stub.read_text()
            content = content.replace(
                "import agent_pb2 as agent__pb2",
                "from . import agent_pb2 as agent__pb2",
            )
            grpc_stub.write_text(content)
