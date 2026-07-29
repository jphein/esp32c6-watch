# trackermeter — per-screen dependency-node cost for Slint

lunameter counts what a frame **draws**. This counts what a frame **binds**, over the
same screens, on the host.

```bash
tools/trackermeter/measure.sh                              # this checkout
WATCH_UI_ROOT=/other/checkout tools/trackermeter/measure.sh # A/B another tree
TRACKERMETER_STAGE=/tmp/tm tools/trackermeter/measure.sh    # pin for build reuse
```

Host build only. Touches no hardware, opens no serial port. Like lunameter it is
**not** a `fambuild` target: it has to build *and run* locally to emit frames, so the
"compile on familiar" rule does not apply to it.

## Why it exists

Story's CHARACTER page cost ~10.7 KB of device heap that lunameter could not see.
`items` and `tex` stayed pinned at 256/256 while that 10.7 KB disappeared, and the
device histogram showed ~675 blocks at **16 B** with no large allocation anywhere.
Trimming rows to reduce scene items would have moved a number that was never the
problem.

That size class has exactly one source. Every time a binding under evaluation reads
a property, Slint appends a dependency node (`i-slint-core/properties.rs`):

```
Property::get -> register_as_dependency_to_current_binding
  -> BindingHolder::register_self_as_dependency
    -> dep_nodes.push_front(DependencyNode::new(..))   // one raw allocation
```

`SingleLinkedListPinNode<DependencyNode<*const BindingHolder>>` is the list node's
`next` plus the node's `next`/`prev`/`binding` — four pointers. **16 B on riscv32,
32 B on x86_64.** The *count* is architecture-independent, so the host can price the
device: `device bytes = host node count x 16`.

`push_front` is also where it fails. `properties.rs:63` is
`assert!(!mem.is_null(), "allocation failed")` — the panic a watch took today with
main at 1,940 B, followed by a second OOM inside the panic printer.

Two properties of the engine that decide how you optimise it:

* **No dedup.** The registration is unconditional per `get()`, so reading the same
  property twice in one expression allocates twice. Measured on eleven equipment
  rows: `i < 6 ? i : i - 6` costs **+22 nodes** over `mod(i, 6)` for an identical
  result.
* **Constant properties are free.** `const_sentinel()` short-circuits the
  registration, so a literal — or an expression over literals, or `root.width` on a
  fixed-size window — registers nothing. This is a trap as much as a lever: swapping
  a literal `366px` back to `root.width - 2 * Theme.safe-side` changed the node count
  by **zero**, because `root.width` is itself constant. What that swap did cost was a
  binding holder per row. Do not assume a property read is a node; measure it.

## What it prints

One `TRACKER` line per frame, in lunameter's shape so the two can be pasted together
and diffed the same way. The renderer fork is reused, so a single run reports both
costs per frame:

```
--- FRAME story(page3,len24) ---
LUNAMETER items=230 textures=220 rounded=8 ...
TRACKER nodes=1694 riscv32_node_bytes=27104 live_blocks=3126 | HOSTsizeof_NOT_DEVICE node=32
```

`nodes` is the **live count for the whole tree** at that frame — resident cost, which
is what the device's heap actually holds — not a per-screen delta.

**Do not read adjacent frames as a per-screen cost.** A binding's nodes are cleared
and rebuilt only when it is re-evaluated, so a frame inherits nodes from bindings that
are still clean. That makes neighbouring frames non-monotonic (`len08` prints 1711
where `len24` prints 1694) while each frame stays perfectly reproducible. The
supported method is **A/B**: run the same frame list against two trees with
`WATCH_UI_ROOT` and diff, exactly as lunameter documents for scene counts.

Determinism is the property that makes that work, and it is checked: two consecutive
runs of the same tree produce byte-identical counts across all 39 frames.

## Provenance — why these numbers are trustworthy

The device figure and the host figure were reached by instruments with nothing in
common, and they agree:

| evidence | figure |
|---|---|
| hardware, region free-space delta on opening CHAR | 10,734 B |
| hardware, allocator hook counters, same screen | 10,800 B |
| this tool, 564 nodes x 16 B | **9,024 B** |

The host figure is the dependency-node share of a total that also contains binding
holders and item-tree allocations, which is why it lands under the device's number
rather than on it.

Two sharper checks, because agreement on one number can be luck:

* **Point prediction.** The engine model says an unknown slot's
  `known ? value : "—"` takes the literal branch and never reads `value`, so
  populating 17 slots should add exactly one node per row. Predicted **+17**,
  measured **+17** (`page3,empty` → `page3,len06`).
* **A/B reproduces a known win.** Against the tree at `61f2f4f` (the commit before
  the windowed CHARACTER page) versus after it:

  | frame | pre-fix | post-fix | delta |
  |---|---|---|---|
  | `story(page3,empty)` | 2145 | 1688 | **−457** (−7,312 B) |
  | `story(page3,len24)` | 2162 | 1694 | −468 (−7,488 B) |
  | `story(page2)` | 1738 | 1734 | −4 |
  | `watchface(page0,closed)` | 1460 | 1460 | **±0** |

  −457 is the same number an independent open-vs-closed delta method produced
  (564 − 107), to the node. `watchface` at exactly ±0 is the control: screens outside
  the change do not move. The −4 on `story(page2)` is real, not noise — the Story
  pages share one component, so a root-level property change shifts all of them
  slightly.

## First intended use, and a warning about how to scope it

`y: (parent.height - self.height) / 2` was the single largest cost on the CHARACTER
page — larger than the row count. `self.height` on a `Text` is itself a computed
binding chain, so centring that way cost **96 nodes** on six rows where a literal box
height plus `vertical-alignment: center` costs none and is pixel-identical.

The idiom appears ~182 times across 25 files, and the instinct is to grep and fix
them all. Most of those instances are free: outside a repeater the idiom costs one
binding chain once. **But which instances multiply is not decidable lexically.** A
component whose *definition* contains the idiom multiplies wherever it is
instantiated inside a repeater — `AppIcon` carries one and sits in the launcher grid;
`Seg` carries two across 15 instantiations; `StatRow` carries two across 23. A
lexical "inside a `for` body" scan finds 14 and misses those entirely.

So the sequence is: measure, fix only what pays, measure again. That is the whole
reason this tool is committed rather than described — the project has repeatedly spent
effort optimising numbers nobody was reading.

## How it stays honest

`instrument.py` **derives** the harness from `tools/lunameter/src/main.rs` on every
run and asserts every patch anchor. There is no second frame list, so adding a screen
to lunameter measures it here for free, and a restructured harness fails loudly
instead of quietly measuring a stale copy — the same contract
`tools/lunameter/instrument.py` holds with the vendored renderer. The generated file
carries a do-not-edit header; the lunameter harness is the one to change.

Staging is unique per run by default. A fixed path races itself: two concurrent runs
`rm -rf` each other's tree and one dies with "cannot remove …: Directory not empty",
observed live with several agents measuring in parallel. `TRACKERMETER_STAGE` pins it
for anyone who wants build reuse and knows they are the only runner.

The allocator only counts — every request is delegated to the system allocator — so
the scene counts reported from the same run are unaffected.
