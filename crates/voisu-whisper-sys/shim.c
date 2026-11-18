// Thin C layer over whisper.cpp so Rust never mirrors `whisper_full_params`
// or `whisper_context_params` by hand. Those structs have ~60 fields with
// nested anonymous structs and change between tags; here the C compiler
// checks every field name against the vendored header instead.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#include "ggml.h"
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

// Debug chatter is dropped; everything else goes to stderr, never stdout,
// because stdout carries protocol frames only.
static void voisu_log_to_stderr(enum ggml_log_level level, const char * text, void * user_data) {
    (void) user_data;
    if (level == GGML_LOG_LEVEL_DEBUG || text == NULL) {
        return;
    }
    fputs(text, stderr);
}

void voisu_whisper_route_logs_to_stderr(void) {
    whisper_log_set(voisu_log_to_stderr, NULL);
    ggml_log_set(voisu_log_to_stderr, NULL);
}

struct whisper_context * voisu_whisper_load(const char * model_path, bool use_gpu, int32_t gpu_device) {
    struct whisper_context_params params = whisper_context_default_params();
    params.use_gpu = use_gpu;
    params.gpu_device = gpu_device;
    // Flash attention looped under greedy decoding in the 2026-09 bench.
    params.flash_attn = false;
    return whisper_init_from_file_with_params(model_path, params);
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
