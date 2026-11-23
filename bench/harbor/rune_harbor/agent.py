"""A Harbor installed agent that runs Rune in the task container.

It installs a static Rune binary (an uploaded one, or a release archive whose
checksum is checked), runs `rune ask --jsonl`, and turns the log into Harbor's
token and cost fields and an ATIF trajectory. The import paths follow Harbor's
own fx adapter; check them against the installed Harbor on the first run.
"""

import shlex
from pathlib import Path

from harbor.agents.agent_auth_resolution import (
    ModelApiFormat,
    ModelConnectionSpec,
    ProviderAccess,
)
from harbor.agents.installed.base import (
    AgentCapabilities,
    BaseInstalledAgent,
    with_prompt_template,
)
from harbor.models.trajectories import Trajectory, format_trajectory_json
from harbor.paths import EnvironmentPaths

from rune_harbor import parse

RELEASES = "https://github.com/AadiXC0DE/Rune/releases/download"
LOG_NAME = "rune.jsonl"


class Rune(BaseInstalledAgent):
    """Rune, run headless with full access inside the task container."""

    capabilities = AgentCapabilities(base_url=True, atif=True)
    MODEL_CONNECTION = ModelConnectionSpec(
        inject_default_base_url=True,
        default_provider="openai",
        agent_provider_access=ProviderAccess(
            api_key_candidate_names=["RUNE_API_KEY", "OPENAI_API_KEY"],
            base_url_candidate_names=["RUNE_BASE_URL", "OPENAI_BASE_URL"],
        ),
        api_key_destination_names={"default": "RUNE_API_KEY"},
        base_url_destination_names={"default": "RUNE_BASE_URL"},
        supported_api_formats=[ModelApiFormat.OPENAI_CHAT_COMPLETIONS],
        default_api_format_by_provider={"default": ModelApiFormat.OPENAI_CHAT_COMPLETIONS},
    )
    # The first release with static Linux archives.
    DEFAULT_VERSION = "0.3.0"

    def __init__(self, *args, binary: str | None = None, **kwargs):
        super().__init__(*args, **kwargs)
        self._binary = binary
        self._instruction: str | None = None

    @staticmethod
    def name() -> str:
        return "rune"

    def get_version_command(self) -> str | None:
        return "rune --version"

    def _release_version(self) -> str:
        return self.version() or self.DEFAULT_VERSION

    async def install(self, environment) -> None:
        await self.ensure_system_dependencies(
            environment, ("curl", "ca_certificates", "tar", "coreutils"))
        if self._binary:
            await environment.upload_file(self._binary, "/tmp/rune-upload")
            await self.exec_as_root(
                environment,
                "install -m 755 /tmp/rune-upload /usr/local/bin/rune && rune --version",
            )
            return
        version = shlex.quote(self._release_version())
        script = (
            "set -eu; arch=$(uname -m); v={v}; "
            "a=rune-$v-$arch-unknown-linux-musl.tar.gz; cd /tmp; "
            "curl -fsSLO {base}/v$v/$a; curl -fsSLO {base}/v$v/$a.sha256; "
            "sha256sum -c $a.sha256; tar -xzf $a -C /usr/local/bin rune; "
            "chmod 755 /usr/local/bin/rune; rune --version"
        ).format(v=version, base=RELEASES)
        await self.exec_as_root(environment, script)

    def _model_id(self) -> str:
        # As fx does: a provider-qualified name keeps only the model part.
        model = self.model_name or ""
        return model.split("/", 1)[1] if "/" in model else model

    @with_prompt_template
    async def run(self, instruction: str, environment, context) -> None:
        self._instruction = instruction
        env = {
            **self.resolve_env_vars(),
            **self.model_connection.env,
            "RUNE_PROVIDER": "chat_completions",
            "RUNE_MODEL": self._model_id(),
            "RUNE_API_KEY_ENV": "RUNE_API_KEY",
            # Whatever the task sets reaches its commands; secret-looking
            # names still pass only when listed exactly.
            "RUNE_SHELL_ENV": "*",
        }
        log = EnvironmentPaths.agent_dir / LOG_NAME
        # No pipefail: a failed run still reaches the verifier, as with fx.
        command = (
            "rune --permission-mode full-access --allow-unsandboxed --network "
            "ask --jsonl --no-save -- {prompt} < /dev/null | tee {log}"
        ).format(prompt=shlex.quote(instruction), log=shlex.quote(str(log)))
        await self.exec_as_agent(environment, command, env=env)

    def convert_trajectory(self, logs_dir: Path) -> Trajectory | None:
        records = parse.read_records(Path(logs_dir) / LOG_NAME)
        data = parse.trajectory_dict(
            records,
            instruction=self._instruction or "",
            model_name=self.model_name,
            version=self.version(),
        )
        return None if data is None else Trajectory.model_validate(data)

    def populate_context_post_run(self, context) -> None:
        trajectory = self.convert_trajectory(self.logs_dir)
        if trajectory is not None:
            (Path(self.logs_dir) / "trajectory.json").write_text(
                format_trajectory_json(trajectory.to_json_dict()))
        totals = parse.usage_totals(parse.read_records(Path(self.logs_dir) / LOG_NAME))
        if totals is None:
            return
        context.n_input_tokens = totals["input_tokens"]
        context.n_cache_tokens = totals["cached_tokens"]
        context.n_output_tokens = totals["output_tokens"]
        context.cost_usd = totals["cost_usd"]
