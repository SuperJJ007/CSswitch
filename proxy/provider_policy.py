"""Provider 策略纯函数 + 状态结构（S1a：从 csswitch_proxy 下沉，改显式收 ProviderState，不读全局）。

依赖方向：本模块是最底层，不 import csswitch_proxy / anthropic_compat（无循环依赖）。
compat 三入口只吃 ProviderState，骨架从模块全局一次性组装后传入。
"""
import random
import re
from dataclasses import dataclass
from typing import Callable

_DATE_SUFFIX = re.compile(r"-\d{8}$")

RULE_PROVIDER_RELAY_FORCE_MODEL_SHELL = "provider.relay.force-model-shell"
RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED = "provider.kimi.relay-thinking-enabled"
RULE_PROVIDER_DASHSCOPE_RESPONSES_TOOLS_CAP = "provider.dashscope.responses-tools-cap"
RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_FILTER = "tool.kimi.web_search.server-tool-filter"
RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE = "tool.relay.input-schema-normalize"
RULE_TOOL_DEEPSEEK_FORCED_TOOL_CHOICE_DISABLE_THINKING = (
    "tool.deepseek.forced-tool-choice-disable-thinking"
)
RULE_TOOL_DASHSCOPE_RESPONSES_WEB_SEARCH_DROP = "tool.dashscope.responses.web_search-drop"


@dataclass(frozen=True)
class Policy:
    """provider 的模型/上下文策略字段子结构。骨架 / 启动字段（url / auth_style / key_env /
    models_url / mode / dsml_capable）绝不进这里，防污染 S1b 跨语言接缝。"""
    passthrough: bool
    force_model_override: bool
    default_model: str
    model_map: dict
    models: list
    model_caps: dict
    default_cap: object          # int 或 None


def policy_from_prov(prov):
    """从运行时 PROV dict 精确提取策略字段成 Policy。"""
    return Policy(
        passthrough=bool(prov.get("passthrough")),
        force_model_override=bool(prov.get("force_model_override")),
        default_model=prov.get("default_model"),
        model_map=prov.get("model_map") or {},
        models=prov.get("models") or [],
        model_caps=prov.get("model_caps") or {},
        default_cap=prov.get("default_cap"),
    )


def _default_nonce():
    """默认 nonce 工厂：等价于旧 id(areq) 随机（rewrite tool_use id 本就随机，
    golden 不录 rewrite body；测试注入固定工厂做精确断言）。"""
    return format(random.getrandbits(24), "x")


@dataclass
class ProviderState:
    """compat 三入口只吃它，不读全局。骨架从模块全局一次性组装后传入。"""
    policy: Policy
    prov_name: str
    relay_force_model: object     # str 或 None
    relay_models: list
    relay_thinking: object         # str 或 None
    shim_mode: str
    nonce_factory: Callable[[], str] = _default_nonce


def _snap_relay_model(name, relay_models):
    """relay 透传：把请求模型贴合到中转站真实 id。精确命中优先；否则找一个以
    请求名为前缀的上游 id；都不中就原样返回。"""
    if not relay_models or name in relay_models:
        return name
    for mid in relay_models:
        if mid.startswith(name + "-") or mid == name:
            return mid
    return name


def resolve_model(name, state):
    """把 Science 传来的模型名解析成当前 provider 的目标模型。
    优先：强制模型 override > 选择器选中名 > 显式映射 > 去日期后缀 > 前缀匹配 > 默认。"""
    p = state.policy
    if p.force_model_override and state.relay_force_model:
        return state.relay_force_model
    if not name:
        return p.default_model
    if p.passthrough:
        return _snap_relay_model(name, state.relay_models)
    mm = p.model_map
    if name in mm:
        return mm[name]
    ids = {m[0] for m in p.models}
    if name in ids:
        return name
    stripped = _DATE_SUFFIX.sub("", name)
    if stripped in mm:
        return mm[stripped]
    for k, v in mm.items():
        if name.startswith(k) or stripped.startswith(k):
            return v
    return p.default_model


def _enabled_budget(max_tokens):
    """thinking:enabled 需 budget_tokens，且必须 < max_tokens（留 token 给输出）。"""
    default = 1024
    if isinstance(max_tokens, int) and max_tokens > 0:
        return max(1, min(default, max_tokens - 1))
    return default


def _append_rule_id(rule_ids, rule_id):
    if rule_ids is not None and rule_id not in rule_ids:
        rule_ids.append(rule_id)


def normalize_thinking(body, prov_name, relay_thinking=None, rule_ids=None):
    """thinking 归一化（纯函数，签名与语义与旧 csswitch_proxy 版一致）。
      (A) 强制 tool_choice(any/tool) → disabled：仅 deepseek。
      (B) relay 的 thinking 策略：enabled（如 Kimi）/ adaptive|None（默认，如 MiniMax）。
          Kimi thinking 模型拒绝“thinking enabled + 指定 tool_choice”组合；保留 tools，
          去掉指定选择，让模型自主触发 tool_use（官方多步工具调用范式 + 真机验证）。
      (C) deepseek 非强制 + auto → adaptive。原地修改并返回 body。"""
    tc = body.get("tool_choice")
    forcing = isinstance(tc, dict) and tc.get("type") in ("any", "tool")
    if forcing and prov_name == "deepseek":
        _append_rule_id(rule_ids, RULE_TOOL_DEEPSEEK_FORCED_TOOL_CHOICE_DISABLE_THINKING)
        body["thinking"] = {"type": "disabled"}
        return body
    if prov_name == "relay" and relay_thinking == "enabled":
        if forcing:
            body.pop("tool_choice", None)
        th = body.get("thinking")
        if not (isinstance(th, dict) and th.get("type") == "enabled"):
            body["thinking"] = {"type": "enabled",
                                "budget_tokens": _enabled_budget(body.get("max_tokens"))}
        return body
    th = body.get("thinking")
    if isinstance(th, dict) and th.get("type") == "auto":
        th = dict(th)
        th["type"] = "adaptive"
        body["thinking"] = th
    return body


def clamp_max_tokens(v, model, state):
    if not v:
        return v
    caps = state.policy.model_caps or {}
    cap = caps.get(model, state.policy.default_cap)
    if cap:
        return min(int(v), cap)
    return v
