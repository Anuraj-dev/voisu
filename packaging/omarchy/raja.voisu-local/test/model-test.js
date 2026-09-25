"use strict"

const assert = require("node:assert/strict")
const Model = require("../Model.js")

const CATALOG = [
  "nemotron-3.5-0.6b",
  "parakeet-tdt-0.6b-v3",
  "qwen3-asr-0.6b",
  "qwen3-asr-1.7b",
  "fun-asr-nano-2512",
  "moonshine-streaming-medium",
  "cohere-transcribe-03-2026"
]

let failed = 0
let passed = 0

function test(name, fn) {
  try {
    fn()
    passed += 1
    console.log("ok  " + name)
  } catch (error) {
    failed += 1
    console.error("not ok  " + name)
    console.error("  " + (error && error.stack ? error.stack : error))
  }
}

function registered(modelId, extra) {
  const row = {
    model_id: modelId,
    runtime: "gguf",
    languages: ["en"],
    devices: ["cuda:0"],
    memory_bytes: 1,
    vram_bytes: 1,
    selected: false,
    ready: false,
    manifest_hash: "abc",
    error: null
  }
  if (extra) Object.assign(row, extra)
  return row
}

function unregistered(modelId, error) {
  return {
    model_id: modelId,
    runtime: "gguf",
    languages: ["en"],
    devices: ["cpu"],
    memory_bytes: 0,
    vram_bytes: 0,
    selected: false,
    ready: false,
    manifest_hash: null,
    error: error || "No such file or directory (os error 2)"
  }
}

test("catalog lists the seven development model ids", () => {
  assert.deepEqual(Model.catalogIds(), CATALOG)
  assert.equal(Model.CATALOG_IDS.length, 7)
})

test("voisuBinary defaults to the development release binary", () => {
  assert.equal(Model.defaultVoisuBinary(), "/home/raja/Anuraj-dev/voisu/target/release/voisu")
  assert.equal(Model.voisuBinary(""), Model.DEFAULT_VOISU_BINARY)
  assert.equal(Model.voisuBinary(null), Model.DEFAULT_VOISU_BINARY)
  assert.equal(Model.voisuBinary("  /tmp/voisu  "), "/tmp/voisu")
  assert.deepEqual(Model.statusCommand(null), [
    Model.DEFAULT_VOISU_BINARY,
    "local-dev",
    "status",
    "--json"
  ])
  assert.deepEqual(Model.selectCommand("/tmp/voisu", CATALOG[0]), [
    "/tmp/voisu",
    "local-dev",
    "select",
    CATALOG[0]
  ])
})

test("parseStatus always returns the seven catalog rows", () => {
  const parsed = Model.parseStatus(JSON.stringify({
    production_selection: null,
    models: [registered(CATALOG[0], { selected: true })]
  }))
  assert.equal(parsed.models.length, 7)
  assert.deepEqual(parsed.models.map((row) => row.model_id), CATALOG)
  assert.equal(parsed.selectedId, CATALOG[0])
  assert.equal(Model.selectedCount(parsed), 1)
  assert.equal(parsed.models[0].registered, true)
  assert.equal(parsed.models[0].selected, true)
  assert.equal(parsed.models[1].registered, false)
  assert.equal(parsed.models[1].error, "unregistered")
})

test("parseStatus keeps unregistered rows visible with their error text", () => {
  const parsed = Model.parseStatus(JSON.stringify({
    models: [
      registered(CATALOG[1], { selected: true }),
      unregistered(CATALOG[2], "pin mismatch")
    ]
  }))
  const qwen = Model.findModel(parsed, CATALOG[2])
  assert.equal(qwen.registered, false)
  assert.equal(qwen.selected, false)
  assert.equal(qwen.error, "pin mismatch")
  assert.equal(Model.decideSelect(qwen).ok, false)
  assert.equal(Model.decideSelect(qwen).error, "pin mismatch")
})

test("exclusive select: malformed JSON never yields two selected rows", () => {
  const parsed = Model.parseStatus(JSON.stringify({
    models: [
      registered(CATALOG[0], { selected: true }),
      registered(CATALOG[3], { selected: true }),
      registered(CATALOG[5], { selected: true })
    ]
  }))
  assert.equal(Model.selectedCount(parsed), 1)
  assert.equal(parsed.selectedId, CATALOG[0])
  assert.equal(parsed.models.filter((row) => row.selected).length, 1)
  assert.equal(Model.barLabel(parsed), "Nemotron")
})

test("exclusive select prefers catalog order, not JSON order", () => {
  const parsed = Model.parseStatus(JSON.stringify({
    models: [
      registered(CATALOG[4], { selected: true }),
      registered(CATALOG[1], { selected: true })
    ]
  }))
  assert.equal(parsed.selectedId, CATALOG[1])
  assert.equal(Model.barLabel(parsed), "Parakeet")
})

test("bar shows Local when none is selected", () => {
  const empty = Model.parseStatus("not json")
  assert.equal(empty.selectedId, null)
  assert.equal(Model.selectedCount(empty), 0)
  assert.equal(Model.barLabel(empty), "Local")
  assert.equal(Model.barLabel(Model.emptyStatus()), "Local")
  assert.equal(Model.parseStatus("[]").models.length, 7)
})

test("unknown ids in JSON are ignored", () => {
  const parsed = Model.parseStatus(JSON.stringify({
    models: [
      registered("not-a-model", { selected: true }),
      registered(CATALOG[6], { selected: true })
    ]
  }))
  assert.equal(parsed.models.length, 7)
  assert.equal(parsed.selectedId, CATALOG[6])
  assert.equal(Model.findModel(parsed, "not-a-model"), null)
})

test("decideSelect refuses unregistered and unknown models", () => {
  const parsed = Model.parseStatus(JSON.stringify({
    models: [unregistered(CATALOG[0], "No such file or directory (os error 2)")]
  }))
  const refused = Model.decideSelect(parsed.models[0])
  assert.equal(refused.ok, false)
  assert.match(refused.error, /No such file/)
  const unknown = Model.decideSelect({ model_id: "nope", manifest_hash: "x", error: null })
  assert.equal(unknown.ok, false)
  assert.match(unknown.error, /unknown development model/)
})

test("decideSelect allows a registered model and does not invent a store", () => {
  const row = registered(CATALOG[2])
  const decision = Model.decideSelect(row)
  assert.equal(decision.ok, true)
  assert.equal(decision.modelId, CATALOG[2])
  assert.equal(decision.error, null)
})

test("busy Recording select surfaces the CLI error and keeps the previous selection", () => {
  const previous = Model.parseStatus(JSON.stringify({
    models: [registered(CATALOG[0], { selected: true }), registered(CATALOG[1])]
  }))
  assert.equal(previous.selectedId, CATALOG[0])
  const outcome = Model.applySelectOutcome(previous, CATALOG[1], {
    code: 1,
    stderr: Model.BUSY_ERROR + "\n",
    stdout: ""
  })
  assert.equal(outcome.ok, false)
  assert.equal(outcome.refresh, false)
  assert.equal(outcome.busy, true)
  assert.equal(outcome.error, Model.BUSY_ERROR)
  assert.equal(outcome.selectedId, CATALOG[0])
  assert.equal(Model.barLabel(previous), "Nemotron")
})

test("successful select asks the UI to refresh CLI status instead of writing a store", () => {
  const previous = Model.parseStatus(JSON.stringify({
    models: [registered(CATALOG[0], { selected: true }), registered(CATALOG[1])]
  }))
  const outcome = Model.applySelectOutcome(previous, CATALOG[1], {
    code: 0,
    stdout: "selected " + CATALOG[1] + " abc\n",
    stderr: ""
  })
  assert.equal(outcome.ok, true)
  assert.equal(outcome.refresh, true)
  assert.equal(outcome.error, null)
  assert.equal(outcome.busy, false)
  assert.equal(outcome.selectedId, CATALOG[0])
})

test("short labels cover every catalog id", () => {
  for (const id of CATALOG) {
    assert.notEqual(Model.shortLabel(id), "Local")
    assert.ok(Model.shortLabel(id).length > 0)
  }
})

console.log(passed + " passed, " + failed + " failed")
if (failed > 0) process.exit(1)
