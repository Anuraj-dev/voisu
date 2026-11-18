// Thin C layer over whisper.cpp so Rust never mirrors `whisper_full_params`
// or `whisper_context_params` by hand. Those structs have ~60 fields with
// nested anonymous structs and change between tags; here the C compiler
// checks every field name against the vendored header instead.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "ggml.h"
#include "ggml-backend.h"
#include "whisper.h"

// Mirrored by `VoisuWhisperOptions` in src/lib.rs; `voisu_whisper_options_size`
// lets a Rust test catch a drift.
struct voisu_whisper_options {
    int32_t n_threads;
    // 1 selects greedy decoding; above 1 selects beam search of that width.
    int32_t beam_size;
    float no_speech_thold;
    const char * language;
    // NULL means no prompt.
    const char * initial_prompt;
    ggml_abort_callback abort_callback;
    void * abort_callback_user_data;
};

size_t voisu_whisper_options_size(void) {
    return sizeof(struct voisu_whisper_options);
}

// whisper_backend_init_gpu falls back to the CPU without telling its caller
// when the GPU device is missing or will not initialize; its log lines are
// the only signal. Classifies one line: 1 for "using <dev> backend" (logged
// just before the device init), -1 for "failed to initialize" or "no GPU
// found", 0 for anything else. Tied to the vendored whisper.cpp tag; a Rust
// test pins the strings.
int32_t voisu_whisper_gpu_init_log_kind(const char * text) {
    static const char using_prefix[] = "whisper_backend_init_gpu: using ";
    static const char failed_prefix[] = "whisper_backend_init_gpu: failed to initialize ";
    static const char none_prefix[] = "whisper_backend_init_gpu: no GPU found";
    if (text == NULL) {
        return 0;
    }
    if (strncmp(text, using_prefix, sizeof(using_prefix) - 1) == 0) {
        return 1;
    }
    if (strncmp(text, failed_prefix, sizeof(failed_prefix) - 1) == 0
            || strncmp(text, none_prefix, sizeof(none_prefix) - 1) == 0) {
        return -1;
    }
    return 0;
}

// What the GPU init logged during the current voisu_whisper_load: 0 nothing,
// 1 a device was chosen, -1 the init failed (sticky). One loader at a time:
// voisu-local-asr serializes every voisu_whisper_load behind LOAD_LOCK.
static int32_t voisu_gpu_init_seen = 0;

// Debug chatter is dropped; everything else goes to stderr, never stdout,
// because stdout carries protocol frames only.
static void voisu_log_to_stderr(enum ggml_log_level level, const char * text, void * user_data) {
    (void) user_data;
    if (level == GGML_LOG_LEVEL_DEBUG || text == NULL) {
        return;
    }
    const int32_t kind = voisu_whisper_gpu_init_log_kind(text);
    if (kind < 0 || (kind > 0 && voisu_gpu_init_seen == 0)) {
        voisu_gpu_init_seen = kind;
    }
    fputs(text, stderr);
}

void voisu_whisper_route_logs_to_stderr(void) {
    whisper_log_set(voisu_log_to_stderr, NULL);
    ggml_log_set(voisu_log_to_stderr, NULL);
}

// ggml is built with GGML_BACKEND_DL: nothing is registered until the
// backend modules (libggml-cpu-<variant>.so, libggml-vulkan.so) are loaded
// from `dir`. ggml scores every CPU variant against this CPU and keeps the
// best one it can run. Returns the number of registered devices.
size_t voisu_whisper_load_backends(const char * dir) {
    ggml_backend_load_all_from_path(dir);
    return ggml_backend_dev_count();
}

// True when a CPU backend variant loaded: the lowest variant (x64) needs no
// SIMD, so false means the variants are missing, not that the CPU is too old.
bool voisu_whisper_cpu_backend_loaded(void) {
    return ggml_backend_reg_by_name("CPU") != NULL;
}

// Mirrored by `VoisuGpu` in src/lib.rs.
struct voisu_gpu {
    // The `gpu_device` index whisper.cpp counts over GPU and iGPU devices.
    int32_t index;
    bool discrete;
    size_t free_bytes;
    size_t total_bytes;
    char name[128];
    char description[256];
};

size_t voisu_gpu_size(void) {
    return sizeof(struct voisu_gpu);
}

// Picks the first discrete GPU, else the first integrated one, counting the
// way whisper_backend_init_gpu counts. Returns false when there is none.
bool voisu_whisper_pick_gpu(struct voisu_gpu * out) {
    int32_t counted = 0;
    bool found = false;
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        ggml_backend_dev_t dev = ggml_backend_dev_get(i);
        const enum ggml_backend_dev_type type = ggml_backend_dev_type(dev);
        if (type != GGML_BACKEND_DEVICE_TYPE_GPU && type != GGML_BACKEND_DEVICE_TYPE_IGPU) {
            continue;
        }
        const bool discrete = type == GGML_BACKEND_DEVICE_TYPE_GPU;
        if (!found || (discrete && !out->discrete)) {
            out->index = counted;
            out->discrete = discrete;
            ggml_backend_dev_memory(dev, &out->free_bytes, &out->total_bytes);
            snprintf(out->name, sizeof(out->name), "%s", ggml_backend_dev_name(dev));
            snprintf(out->description, sizeof(out->description), "%s", ggml_backend_dev_description(dev));
            found = true;
        }
        counted++;
    }
    return found;
}

// `gpu_ready` reports whether the GPU backend initialized; with use_gpu it
// is false when whisper.cpp quietly fell back to the CPU.
struct whisper_context * voisu_whisper_load(
        const char * model_path,
        bool use_gpu,
        int32_t gpu_device,
        bool flash_attn,
        bool * gpu_ready) {
    struct whisper_context_params params = whisper_context_default_params();
    params.use_gpu = use_gpu;
    params.gpu_device = gpu_device;
    params.flash_attn = flash_attn;
    // The GPU check reads whisper.cpp's log lines, so they must reach us.
    voisu_whisper_route_logs_to_stderr();
    voisu_gpu_init_seen = 0;
    struct whisper_context * ctx = whisper_init_from_file_with_params(model_path, params);
    *gpu_ready = use_gpu && voisu_gpu_init_seen == 1;
    return ctx;
}

// Decodes one window with no carried context. Returns whisper_full's status:
// 0 on success, non-zero on failure or abort.
int32_t voisu_whisper_transcribe(
        struct whisper_context * ctx,
        const struct voisu_whisper_options * options,
        const float * samples,
        int32_t n_samples) {
    const bool beam = options->beam_size > 1;
    struct whisper_full_params params =
        whisper_full_default_params(beam ? WHISPER_SAMPLING_BEAM_SEARCH : WHISPER_SAMPLING_GREEDY);
    params.n_threads = options->n_threads;
    params.no_context = true;
    params.print_special = false;
    params.print_progress = false;
    params.print_realtime = false;
    params.print_timestamps = false;
    params.language = options->language;
    params.detect_language = false;
    params.suppress_nst = true;
    params.temperature = 0.0f;
    params.temperature_inc = 0.0f;
    params.no_speech_thold = options->no_speech_thold;
    params.greedy.best_of = 1;
    if (beam) {
        params.beam_search.beam_size = options->beam_size;
    }
    params.initial_prompt = options->initial_prompt;
    params.abort_callback = options->abort_callback;
    params.abort_callback_user_data = options->abort_callback_user_data;
    return whisper_full(ctx, params, samples, n_samples);
}
