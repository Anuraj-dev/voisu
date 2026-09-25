// Qt-free local-dev model list for the Omarchy bar plugin.
// Status and selection come only from `voisu local-dev`; this module never
// writes selection.json.

var CATALOG_IDS = [
  "nemotron-3.5-0.6b",
  "parakeet-tdt-0.6b-v3",
  "qwen3-asr-0.6b",
  "qwen3-asr-1.7b",
  "fun-asr-nano-2512",
  "moonshine-streaming-medium",
  "cohere-transcribe-03-2026"
]

var SHORT_LABELS = {
  "nemotron-3.5-0.6b": "Nemotron",
  "parakeet-tdt-0.6b-v3": "Parakeet",
  "qwen3-asr-0.6b": "Qwen 0.6B",
  "qwen3-asr-1.7b": "Qwen 1.7B",
  "fun-asr-nano-2512": "Fun-ASR",
  "moonshine-streaming-medium": "Moonshine",
  "cohere-transcribe-03-2026": "Cohere"
}

var DEFAULT_VOISU_BINARY = "/home/raja/Anuraj-dev/voisu/target/release/voisu"
var BAR_NONE_LABEL = "Local"
var UNREGISTERED_ERROR = "unregistered"
var BUSY_ERROR = "development model selection refused while Recording is active"

function catalogIds() {
  return CATALOG_IDS.slice()
}

function defaultVoisuBinary() {
  return DEFAULT_VOISU_BINARY
}

function voisuBinary(setting) {
  var value = String(setting === undefined || setting === null ? "" : setting).replace(/^\s+|\s+$/g, "")
  return value === "" ? DEFAULT_VOISU_BINARY : value
}

function isCatalogId(modelId) {
  return CATALOG_IDS.indexOf(String(modelId || "")) !== -1
}

function shortLabel(modelId) {
  var id = String(modelId || "")
  return SHORT_LABELS[id] || BAR_NONE_LABEL
}

function busyError() {
  return BUSY_ERROR
}

function isBusyError(message) {
  var text = String(message || "")
  return text.indexOf("Recording") !== -1
}

function isRegistered(row) {
  if (!row || typeof row !== "object") return false
  if (!isCatalogId(row.model_id)) return false
  var error = row.error
  if (error !== undefined && error !== null && String(error) !== "") return false
  var hash = row.manifest_hash
  return hash !== undefined && hash !== null && String(hash) !== ""
}

function exclusiveSelectedId(rows) {
  if (!rows || !rows.length) return null
  var byId = {}
  for (var i = 0; i < rows.length; i++) {
    var row = rows[i]
    if (!row || typeof row !== "object") continue
    var id = String(row.model_id || "")
    if (!isCatalogId(id)) continue
    if (row.selected === true && byId[id] === undefined) byId[id] = true
  }
  for (var j = 0; j < CATALOG_IDS.length; j++) {
    if (byId[CATALOG_IDS[j]]) return CATALOG_IDS[j]
  }
  return null
}

function emptyRow(modelId) {
  return {
    model_id: modelId,
    runtime: "",
    languages: [],
    devices: [],
    memory_bytes: 0,
    vram_bytes: 0,
    selected: false,
    ready: false,
    manifest_hash: null,
    error: UNREGISTERED_ERROR,
    registered: false,
    shortLabel: shortLabel(modelId)
  }
}

function normalizeRow(src, selectedId) {
  var id = String(src && src.model_id ? src.model_id : "")
  var row = emptyRow(id)
  if (!src || typeof src !== "object") return row
  row.runtime = src.runtime == null ? "" : String(src.runtime)
  row.languages = Array.isArray(src.languages) ? src.languages.slice() : []
  row.devices = Array.isArray(src.devices) ? src.devices.slice() : []
  row.memory_bytes = Number(src.memory_bytes) || 0
  row.vram_bytes = Number(src.vram_bytes) || 0
  row.ready = src.ready === true
  row.manifest_hash = src.manifest_hash == null || src.manifest_hash === "" ? null : String(src.manifest_hash)
  if (src.error === undefined || src.error === null || String(src.error) === "") {
    row.error = row.manifest_hash ? null : UNREGISTERED_ERROR
  } else {
    row.error = String(src.error)
  }
  row.registered = isRegistered(row)
  if (!row.registered && (row.error === undefined || row.error === null || row.error === "")) {
    row.error = UNREGISTERED_ERROR
  }
  row.selected = id === selectedId
  return row
}

function parseStatus(raw) {
  var parsed = null
  try {
    parsed = JSON.parse(String(raw === undefined || raw === null ? "" : raw))
  } catch (error) {
    parsed = null
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) parsed = {}
  var incoming = Array.isArray(parsed.models) ? parsed.models : []
  var byId = {}
  for (var i = 0; i < incoming.length; i++) {
    var src = incoming[i]
    if (!src || typeof src !== "object") continue
    var id = String(src.model_id || "")
    if (!isCatalogId(id)) continue
    byId[id] = src
  }
  var selectedId = exclusiveSelectedId(incoming)
  var models = []
  for (var j = 0; j < CATALOG_IDS.length; j++) {
    var catalogId = CATALOG_IDS[j]
    var row = normalizeRow(byId[catalogId] || { model_id: catalogId }, selectedId)
    row.model_id = catalogId
    row.shortLabel = shortLabel(catalogId)
    row.selected = catalogId === selectedId
    models.push(row)
  }
  return {
    production_selection: parsed.production_selection == null ? null : parsed.production_selection,
    selectedId: selectedId,
    models: models
  }
}

function emptyStatus() {
  return parseStatus("{}")
}

function barLabel(status) {
  var id = status && status.selectedId
  if (!id) return BAR_NONE_LABEL
  return shortLabel(id)
}

function selectedCount(status) {
  if (!status || !status.models) return 0
  var count = 0
  for (var i = 0; i < status.models.length; i++) {
    if (status.models[i] && status.models[i].selected === true) count += 1
  }
  return count
}

function findModel(status, modelId) {
  if (!status || !status.models) return null
  var id = String(modelId || "")
  for (var i = 0; i < status.models.length; i++) {
    if (status.models[i] && status.models[i].model_id === id) return status.models[i]
  }
  return null
}

function statusCommand(binary) {
  return [voisuBinary(binary), "local-dev", "status", "--json"]
}

function selectCommand(binary, modelId) {
  return [voisuBinary(binary), "local-dev", "select", String(modelId || "")]
}

function decideSelect(row) {
  var id = row && row.model_id
  if (!isCatalogId(id)) {
    return { ok: false, error: "unknown development model: " + String(id || "") }
  }
  if (!isRegistered(row)) {
    return { ok: false, error: String((row && row.error) || UNREGISTERED_ERROR) }
  }
  return { ok: true, modelId: id, error: null }
}

function applySelectOutcome(previousStatus, modelId, outcome) {
  var previousId = previousStatus && previousStatus.selectedId ? previousStatus.selectedId : null
  var code = outcome && outcome.code
  if (code === 0) {
    return {
      ok: true,
      refresh: true,
      error: null,
      busy: false,
      selectedId: previousId,
      modelId: String(modelId || "")
    }
  }
  var stderr = String((outcome && outcome.stderr) || "")
  var stdout = String((outcome && outcome.stdout) || "")
  var message = (stderr || stdout).replace(/^\s+|\s+$/g, "")
  if (message === "") message = "select failed"
  return {
    ok: false,
    refresh: false,
    error: message,
    busy: isBusyError(message),
    selectedId: previousId,
    modelId: String(modelId || "")
  }
}

if (typeof module !== "undefined") {
  module.exports = {
    CATALOG_IDS: CATALOG_IDS,
    SHORT_LABELS: SHORT_LABELS,
    DEFAULT_VOISU_BINARY: DEFAULT_VOISU_BINARY,
    BAR_NONE_LABEL: BAR_NONE_LABEL,
    UNREGISTERED_ERROR: UNREGISTERED_ERROR,
    BUSY_ERROR: BUSY_ERROR,
    catalogIds: catalogIds,
    defaultVoisuBinary: defaultVoisuBinary,
    voisuBinary: voisuBinary,
    isCatalogId: isCatalogId,
    shortLabel: shortLabel,
    busyError: busyError,
    isBusyError: isBusyError,
    isRegistered: isRegistered,
    exclusiveSelectedId: exclusiveSelectedId,
    emptyRow: emptyRow,
    parseStatus: parseStatus,
    emptyStatus: emptyStatus,
    barLabel: barLabel,
    selectedCount: selectedCount,
    findModel: findModel,
    statusCommand: statusCommand,
    selectCommand: selectCommand,
    decideSelect: decideSelect,
    applySelectOutcome: applySelectOutcome
  }
}
