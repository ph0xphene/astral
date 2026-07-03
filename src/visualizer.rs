//! 3D Code Topology export.
//!
//! Flattens the chunk table into a `{nodes, links}` graph consumable by
//! `3d-force-graph` (Three.js/WebGL). Nodes are function-like definitions
//! plus synthesized state-variable and external-call nodes; links follow
//! `referenced_symbols`. The point is visual anomaly hunting: a hot-red
//! node (low-level call) with many inbound links, a state variable written
//! from unexpected corners, a contract cluster wired to everything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use crate::retrieval::BUILTINS;
use crate::storage::StoredChunk;

// Hacker palette. Red is reserved for the highest-signal nodes: function
// bodies containing low-level calls (`call`/`delegatecall`/...). Unresolved
// external callees are grey — outside the system boundary.
const COLOR_LOW_LEVEL: &str = "#ff0055";
const COLOR_FUNCTION: &str = "#4e7cff";
const COLOR_CONSTRUCTOR: &str = "#ffd166";
const COLOR_MODIFIER: &str = "#8a2be2";
const COLOR_RECEIVE: &str = "#ff8c42";
const COLOR_STATE_VAR: &str = "#00ffcc";
const COLOR_EXTERNAL_CALL: &str = "#94a3b8";

const STATE_VAR_SIZE: f64 = 5.0;
const EXTERNAL_CALL_SIZE: f64 = 6.0;

#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub id: String,
    pub group: &'static str,
    pub size: f64,
    pub color: &'static str,
    /// Structural threat estimate, 0 (inert) to 10 (drop everything and
    /// read this function). See [`risk_score`] for the scoring rules.
    pub risk_score: u8,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphLink {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Serialize)]
pub struct CodeGraph {
    pub nodes: Vec<GraphNode>,
    pub links: Vec<GraphLink>,
}

/// Builds the topology graph from stored chunks. Pure function — all I/O
/// stays in [`write_graph_files`].
pub fn build_graph(chunks: &[StoredChunk]) -> CodeGraph {
    // Function-name index for cross-contract link resolution.
    let mut by_function_name: BTreeMap<&str, Vec<&StoredChunk>> = BTreeMap::new();
    for chunk in chunks {
        by_function_name
            .entry(chunk.function_name.as_str())
            .or_default()
            .push(chunk);
    }

    // Modifier definitions present in the codebase; used both for risk
    // scoring (a guarded function is less exposed) and defined in one place.
    let modifier_names: BTreeSet<&str> = chunks
        .iter()
        .filter(|c| c.kind == "modifier")
        .map(|c| c.function_name.as_str())
        .collect();

    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    let mut links: BTreeSet<GraphLink> = BTreeSet::new();

    for chunk in chunks {
        let id = node_id(chunk);

        let touches_state = chunk.referenced_symbols.iter().any(|s| {
            matches!(
                resolve_symbol(s, chunk, &by_function_name),
                ResolvedTarget::StateVariable(_)
            )
        });

        nodes.insert(
            id.clone(),
            GraphNode {
                id: id.clone(),
                group: group_of(chunk),
                size: definition_size(chunk),
                color: color_of(chunk),
                risk_score: risk_score(chunk, &modifier_names, touches_state),
            },
        );

        for symbol in &chunk.referenced_symbols {
            match resolve_symbol(symbol, chunk, &by_function_name) {
                ResolvedTarget::Definition(target_id) => {
                    if target_id != id {
                        links.insert(GraphLink {
                            source: id.clone(),
                            target: target_id,
                        });
                    }
                }
                ResolvedTarget::StateVariable(name) => {
                    let var_id = format!("{}::{}", chunk.contract_name, name);
                    nodes.entry(var_id.clone()).or_insert(GraphNode {
                        id: var_id.clone(),
                        group: "state_variable",
                        size: STATE_VAR_SIZE,
                        color: COLOR_STATE_VAR,
                        risk_score: 0,
                    });
                    links.insert(GraphLink {
                        source: id.clone(),
                        target: var_id,
                    });
                }
                ResolvedTarget::ExternalCall(callee) => {
                    nodes.entry(callee.clone()).or_insert(GraphNode {
                        id: callee.clone(),
                        group: "external_call",
                        size: EXTERNAL_CALL_SIZE,
                        color: COLOR_EXTERNAL_CALL,
                        risk_score: 0,
                    });
                    links.insert(GraphLink {
                        source: id.clone(),
                        target: callee,
                    });
                }
                ResolvedTarget::Skip => {}
            }
        }
    }

    CodeGraph {
        nodes: nodes.into_values().collect(),
        links: links.into_iter().collect(),
    }
}

/// Serializes the graph to `json_path` and writes a self-contained HTML
/// viewer (3d-force-graph via CDN, data inlined so it opens from `file://`)
/// next to it. Returns the viewer path.
pub fn write_graph_files(
    graph: &CodeGraph,
    json_path: &Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    let json = serde_json::to_string_pretty(graph)?;
    std::fs::write(json_path, &json)?;

    let html_path = json_path.with_extension("html");
    std::fs::write(&html_path, viewer_html(&json))?;
    Ok(html_path)
}

fn node_id(chunk: &StoredChunk) -> String {
    format!("{}::{}", chunk.contract_name, chunk.function_name)
}

fn group_of(chunk: &StoredChunk) -> &'static str {
    match chunk.kind.as_str() {
        "constructor" => "constructor",
        "modifier" => "modifier",
        "fallback_or_receive" => "receive",
        _ => "function",
    }
}

/// Body size drives node size: `sqrt` keeps 300-line monsters from dwarfing
/// everything while single-line getters stay visible.
fn definition_size(chunk: &StoredChunk) -> f64 {
    let lines = (chunk.end_line.saturating_sub(chunk.start_line) + 1) as f64;
    (3.0 + 2.0 * lines.sqrt()).min(40.0)
}

fn color_of(chunk: &StoredChunk) -> &'static str {
    if has_low_level_call(&chunk.code_content) {
        return COLOR_LOW_LEVEL;
    }
    match chunk.kind.as_str() {
        "constructor" => COLOR_CONSTRUCTOR,
        "modifier" => COLOR_MODIFIER,
        "fallback_or_receive" => COLOR_RECEIVE,
        _ => COLOR_FUNCTION,
    }
}

/// Guard modifiers that ship with common base contracts (OpenZeppelin and
/// friends). Their definitions usually aren't in the ingested codebase, so
/// they're recognized by name in addition to locally defined modifiers.
const KNOWN_GUARD_MODIFIERS: &[&str] = &[
    "nonReentrant",
    "onlyOwner",
    "onlyRole",
    "whenNotPaused",
    "whenPaused",
    "initializer",
    "reinitializer",
];

/// Structural threat score, clamped to 0..=10:
/// - low-level call in the body (`call`/`delegatecall`/...): **+4**
/// - `public`/`external`, not `view`/`pure`, touches contract state: **+3**
/// - `fallback`/`receive` entry point: **+2**
/// - guarded by an access modifier (defined locally or a well-known one
///   like `onlyOwner`/`nonReentrant`): **−3**
fn risk_score(chunk: &StoredChunk, modifier_names: &BTreeSet<&str>, touches_state: bool) -> u8 {
    let mut score: i32 = 0;
    let signature = chunk
        .code_content
        .split('{')
        .next()
        .unwrap_or(&chunk.code_content);

    if has_low_level_call(&chunk.code_content) {
        score += 4;
    }
    if touches_state
        && has_keyword(signature, &["public", "external"])
        && !has_keyword(signature, &["view", "pure"])
    {
        score += 3;
    }
    if chunk.kind == "fallback_or_receive" {
        score += 2;
    }

    let guarded = chunk.referenced_symbols.iter().any(|s| {
        modifier_names.contains(s.as_str()) || KNOWN_GUARD_MODIFIERS.contains(&s.as_str())
    });
    if guarded {
        score -= 3;
    }

    score.clamp(0, 10) as u8
}

/// Word-boundary keyword check on a function signature; a substring match
/// would confuse `viewer` with `view`.
fn has_keyword(signature: &str, keywords: &[&str]) -> bool {
    signature
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| keywords.contains(&word))
}

fn has_low_level_call(code: &str) -> bool {
    [
        ".call(",
        ".call{",
        ".delegatecall(",
        ".delegatecall{",
        ".staticcall(",
        "selfdestruct(",
    ]
    .iter()
    .any(|needle| code.contains(needle))
}

enum ResolvedTarget {
    /// Another definition chunk in the table.
    Definition(String),
    /// A state variable of the referencing contract.
    StateVariable(String),
    /// A call whose target has no definition in the table.
    ExternalCall(String),
    Skip,
}

/// Maps one raw `referenced_symbols` entry to a graph target.
///
/// Resolution order for plain identifiers: definition in the same contract,
/// then a definition in exactly one other contract (unambiguous), then —
/// for lowercase/underscore names — a state variable of this contract.
/// Capitalized unresolved names are type casts (`IERC20(x)`) and are
/// skipped. Dotted symbols resolve by method name, falling back to an
/// external-call node (`token.transfer`, `msg.sender.call`).
fn resolve_symbol(
    symbol: &str,
    from: &StoredChunk,
    by_function_name: &BTreeMap<&str, Vec<&StoredChunk>>,
) -> ResolvedTarget {
    let head = symbol.split(['{', '(']).next().unwrap_or(symbol).trim();
    if head.is_empty() {
        return ResolvedTarget::Skip;
    }

    if head.contains('.') {
        let method = head.rsplit('.').next().unwrap_or(head);
        if BUILTINS.contains(&method) {
            return ResolvedTarget::Skip;
        }
        return match lookup(method, from, by_function_name) {
            Some(id) => ResolvedTarget::Definition(id),
            None => ResolvedTarget::ExternalCall(head.to_owned()),
        };
    }

    if BUILTINS.contains(&head) {
        return ResolvedTarget::Skip;
    }
    if let Some(id) = lookup(head, from, by_function_name) {
        return ResolvedTarget::Definition(id);
    }
    if head.starts_with(|c: char| c.is_lowercase() || c == '_') {
        return ResolvedTarget::StateVariable(head.to_owned());
    }
    ResolvedTarget::Skip
}

fn lookup(
    name: &str,
    from: &StoredChunk,
    by_function_name: &BTreeMap<&str, Vec<&StoredChunk>>,
) -> Option<String> {
    let candidates = by_function_name.get(name)?;
    let same_contract = candidates
        .iter()
        .find(|c| c.contract_name == from.contract_name);
    match same_contract {
        Some(chunk) => Some(node_id(chunk)),
        // Ambiguous cross-contract names (`decimals` defined in five
        // contracts) are left unresolved rather than linked to all of them.
        None if candidates.len() == 1 => Some(node_id(candidates[0])),
        None => None,
    }
}

// The viewer renders with `3d-force-graph`, a force-directed layout on top
// of three.js/WebGL. Data is inlined so the file opens straight from
// `file://` — no dev server, no fetch.
fn viewer_html(graph_json: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>astral — code topology</title>
<style>
  body {{ margin: 0; background: #04060f; overflow: hidden; }}
  .panel {{
    position: fixed; z-index: 1; color: #cbd5e1;
    font: 12px/1.7 ui-monospace, monospace;
    background: rgba(4, 6, 15, 0.85); border: 1px solid #1e293b;
    padding: 10px 14px; border-radius: 8px;
  }}
  #hud {{ top: 12px; left: 12px; }}
  #hud h1 {{ margin: 0 0 2px; font-size: 14px; letter-spacing: 4px; color: #e2e8f0; }}
  #hud .stats {{ color: #64748b; }}
  #hud input[type=text] {{
    margin-top: 8px; width: 200px; padding: 4px 8px;
    color: #e2e8f0; background: #0b1120; border: 1px solid #1e293b;
    border-radius: 4px; font: inherit; outline: none;
  }}
  #hud .risk {{ margin-top: 10px; }}
  #hud .risk label {{ display: block; color: #94a3b8; }}
  #hud input[type=range] {{ width: 200px; accent-color: #ff0055; }}
  #hud .risk b {{ color: #ff9100; }}
  #legend {{ bottom: 12px; left: 12px; user-select: none; }}
  #legend div {{ cursor: pointer; }}
  #legend div.off {{ opacity: 0.3; }}
  .dot {{ display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 6px; }}
</style>
<script type="importmap">
  {{
    "imports": {{
      "three": "https://esm.sh/three",
      "three/": "https://esm.sh/three/"
    }}
  }}
</script>
</head>
<body>
<div id="hud" class="panel">
  <h1>ASTRAL</h1>
  <div class="stats" id="stats"></div>
  <input id="search" type="text" placeholder="search nodes…" spellcheck="false">
  <div class="risk">
    <label>minimum risk level: <b id="riskValue">0</b> <span id="riskCount"></span></label>
    <input id="risk" type="range" min="0" max="10" step="1" value="0">
  </div>
</div>
<div id="legend" class="panel"></div>
<div id="graph"></div>
<script type="module">
// `?external=three` makes 3d-force-graph resolve `three` through the import
// map above — one shared three.js instance, so meshes built here render in
// the library's scene without version clashes.
import ForceGraph3D from 'https://esm.sh/3d-force-graph?external=three';
import * as THREE from 'three';

const data = {graph_json};

const GROUPS = [
  ['function',       '#4e7cff', 'function'],
  ['constructor',    '#ffd166', 'constructor'],
  ['modifier',       '#8a2be2', 'modifier'],
  ['receive',        '#ff8c42', 'fallback / receive'],
  ['state_variable', '#00ffcc', 'state variable'],
  ['external_call',  '#94a3b8', 'external call'],
];
const active = new Set(GROUPS.map(g => g[0]));
let term = '';
let minRisk = 0;

const PULSE_THRESHOLD = 7;   // risk_score >= 7 → pulsing threat node
const DIM_COLOR = 'rgba(148, 163, 184, 0.08)';

const visible = n =>
  active.has(n.group) && (!term || n.id.toLowerCase().includes(term));
const hot = n => n.risk_score >= minRisk;

// Pulsing meshes for high-risk nodes, animated below.
const pulsing = [];

const Graph = ForceGraph3D()(document.getElementById('graph'))
  .graphData(data)
  .nodeVal(n => n.size)
  .nodeColor(n => hot(n) ? n.color : DIM_COLOR)
  .nodeLabel(n => `${{n.id}} [${{n.group}}] · risk ${{n.risk_score}}`)
  .nodeOpacity(0.9)
  .nodeThreeObject(n => {{
    if (n.risk_score < PULSE_THRESHOLD) return false; // default sphere
    const radius = 4 * Math.cbrt(n.size);             // match default sizing
    const mesh = new THREE.Mesh(
      new THREE.SphereGeometry(radius, 24, 24),
      new THREE.MeshLambertMaterial({{
        color: '#ff0055', emissive: '#ff0055', emissiveIntensity: 0.7,
        transparent: true, opacity: 0.95,
      }}),
    );
    pulsing.push({{ node: n, mesh, phase: Math.random() * Math.PI * 2 }});
    return mesh;
  }})
  .backgroundColor('#04060f')
  .linkColor(l => hot(l.source) && hot(l.target) ? '#334155' : DIM_COLOR)
  .linkOpacity(0.35)
  .linkDirectionalParticles(1)
  .linkDirectionalParticleWidth(1.2)
  .linkDirectionalParticleColor(() => '#7dd3fc')
  .onNodeClick(node => {{
    // Fly the three.js camera to the clicked node.
    const dist = 80;
    const ratio = 1 + dist / Math.hypot(node.x, node.y, node.z);
    Graph.cameraPosition(
      {{ x: node.x * ratio, y: node.y * ratio, z: node.z * ratio }},
      node, 1000,
    );
  }});

// Threat pulse: high-risk nodes breathe in size and sweep neon-red →
// acid-orange. Dimmed-out ones fade towards transparency smoothly.
const RED = new THREE.Color('#ff0055');
const ORANGE = new THREE.Color('#ff9100');
(function animate() {{
  const t = performance.now() / 1000;
  for (const {{ node, mesh, phase }} of pulsing) {{
    const beat = 0.5 + 0.5 * Math.sin(t * 4 + phase);
    mesh.scale.setScalar(1 + 0.3 * beat);
    mesh.material.color.lerpColors(RED, ORANGE, beat);
    mesh.material.emissive.lerpColors(RED, ORANGE, beat);
    const target = hot(node) ? 0.95 : 0.1;
    mesh.material.opacity += (target - mesh.material.opacity) * 0.15;
  }}
  requestAnimationFrame(animate);
}})();

function refresh() {{
  Graph
    .nodeVisibility(visible)
    .linkVisibility(l => visible(l.source) && visible(l.target))
    .nodeColor(n => hot(n) ? n.color : DIM_COLOR)
    .linkColor(l => hot(l.source) && hot(l.target) ? '#334155' : DIM_COLOR);
  const hotCount = data.nodes.filter(n => visible(n) && hot(n)).length;
  document.getElementById('riskCount').textContent =
    minRisk > 0 ? `(${{hotCount}} nodes in focus)` : '';
}}
refresh();

document.getElementById('stats').textContent =
  `${{data.nodes.length}} nodes · ${{data.links.length}} links`;

document.getElementById('search').addEventListener('input', e => {{
  term = e.target.value.trim().toLowerCase();
  refresh();
}});

document.getElementById('risk').addEventListener('input', e => {{
  minRisk = Number(e.target.value);
  document.getElementById('riskValue').textContent = minRisk;
  refresh();
}});

const legend = document.getElementById('legend');
for (const [group, color, label] of GROUPS) {{
  const row = document.createElement('div');
  row.innerHTML = `<span class="dot" style="background:${{color}}"></span>${{label}}`;
  row.title = 'click to toggle';
  row.onclick = () => {{
    active.has(group) ? active.delete(group) : active.add(group);
    row.classList.toggle('off');
    refresh();
  }};
  legend.appendChild(row);
}}
const note = document.createElement('div');
note.style.cssText = 'margin-top:6px;color:#64748b;cursor:default';
note.innerHTML = '<span class="dot" style="background:#ff0055"></span>low-level call inside';
legend.appendChild(note);
</script>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(
        contract: &str,
        function: &str,
        kind: &str,
        code: &str,
        symbols: &[&str],
        lines: (u32, u32),
    ) -> StoredChunk {
        StoredChunk {
            id: format!("f.sol::{contract}::{function}::{}", lines.0),
            file_path: "f.sol".into(),
            contract_name: contract.into(),
            function_name: function.into(),
            code_content: code.into(),
            referenced_symbols: symbols.iter().map(|s| s.to_string()).collect(),
            kind: kind.into(),
            start_line: lines.0,
            end_line: lines.1,
        }
    }

    fn vault_chunks() -> Vec<StoredChunk> {
        vec![
            chunk(
                "Vault",
                "withdraw",
                "function",
                "function withdraw() { msg.sender.call{value: x}(\"\"); }",
                &[
                    "balances",
                    "msg.sender.call{value: x}",
                    "onlyInit",
                    "token.transfer",
                ],
                (10, 20),
            ),
            chunk(
                "Vault",
                "onlyInit",
                "modifier",
                "modifier onlyInit() { _; }",
                &[],
                (5, 7),
            ),
            chunk(
                "Token",
                "transfer",
                "function",
                "function transfer() {}",
                &[],
                (1, 3),
            ),
        ]
    }

    #[test]
    fn builds_expected_node_groups_and_colors() {
        let graph = build_graph(&vault_chunks());
        let by_id: BTreeMap<&str, &GraphNode> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

        // Low-level call in the body wins over the plain-function color.
        assert_eq!(by_id["Vault::withdraw"].color, COLOR_LOW_LEVEL);
        assert_eq!(by_id["Vault::onlyInit"].group, "modifier");
        assert_eq!(by_id["Vault::onlyInit"].color, COLOR_MODIFIER);
        // `balances` never appears as a definition → state variable node.
        assert_eq!(by_id["Vault::balances"].group, "state_variable");
        assert_eq!(by_id["Vault::balances"].size, STATE_VAR_SIZE);
        // `msg.sender.call` has no definition → external call node.
        assert_eq!(by_id["msg.sender.call"].group, "external_call");
    }

    #[test]
    fn links_resolve_local_cross_contract_and_synthesized_targets() {
        let graph = build_graph(&vault_chunks());
        let has = |s: &str, t: &str| graph.links.iter().any(|l| l.source == s && l.target == t);
        assert!(has("Vault::withdraw", "Vault::balances"));
        assert!(has("Vault::withdraw", "Vault::onlyInit"));
        // `token.transfer` resolves by method name to the unique definition.
        assert!(has("Vault::withdraw", "Token::transfer"));
        assert!(has("Vault::withdraw", "msg.sender.call"));
    }

    #[test]
    fn risk_scores_follow_structural_rules() {
        let chunks = vec![
            // Low-level call (+4), external and writes state (+3), no guard → 7.
            chunk(
                "Vault",
                "sweep",
                "function",
                "function sweep() external { owner.call{value: 1}(\"\"); }",
                &["funds"],
                (1, 3),
            ),
            // Same shape but guarded by a well-known modifier → 7 − 3 = 4.
            chunk(
                "Vault",
                "sweepOwner",
                "function",
                "function sweepOwner() external onlyOwner { owner.call{value: 1}(\"\"); }",
                &["funds", "onlyOwner"],
                (5, 7),
            ),
            // Guard defined locally in the codebase counts too → 3 − 3 = 0.
            chunk(
                "Vault",
                "setFee",
                "function",
                "function setFee(uint256 f) external onlyInit { fee = f; }",
                &["fee", "onlyInit"],
                (9, 11),
            ),
            chunk(
                "Vault",
                "onlyInit",
                "modifier",
                "modifier onlyInit() { _; }",
                &[],
                (13, 15),
            ),
            // Payable entry point → +2.
            chunk(
                "Vault",
                "receive",
                "fallback_or_receive",
                "receive() external payable {}",
                &[],
                (17, 17),
            ),
            // `view` never counts as a state-changing external → 0.
            chunk(
                "Vault",
                "getFee",
                "function",
                "function getFee() external view returns (uint256) { return fee; }",
                &["fee"],
                (19, 21),
            ),
        ];

        let graph = build_graph(&chunks);
        let risk: BTreeMap<&str, u8> = graph
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.risk_score))
            .collect();

        assert_eq!(risk["Vault::sweep"], 7);
        assert_eq!(risk["Vault::sweepOwner"], 4);
        assert_eq!(risk["Vault::setFee"], 0);
        assert_eq!(risk["Vault::receive"], 2);
        assert_eq!(risk["Vault::getFee"], 0);
        // Synthesized nodes carry no structural risk of their own.
        assert_eq!(risk["Vault::funds"], 0);
    }

    #[test]
    fn size_scales_with_line_count_and_every_link_endpoint_exists() {
        let graph = build_graph(&vault_chunks());
        let by_id: BTreeMap<&str, &GraphNode> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        assert!(by_id["Vault::withdraw"].size > by_id["Vault::onlyInit"].size);

        for link in &graph.links {
            assert!(
                by_id.contains_key(link.source.as_str()),
                "dangling {}",
                link.source
            );
            assert!(
                by_id.contains_key(link.target.as_str()),
                "dangling {}",
                link.target
            );
        }
    }
}
