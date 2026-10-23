// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT
#include "parakeet-abi.h"
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    void *library;
    const SherpaOnnxOfflineRecognizer *recognizer;
    const char *(*version)(void);
    const SherpaOnnxOfflineRecognizer *(*create)(const SherpaOnnxOfflineRecognizerConfig *);
    void (*destroy)(const SherpaOnnxOfflineRecognizer *);
    const SherpaOnnxOfflineStream *(*stream)(const SherpaOnnxOfflineRecognizer *);
    void (*destroy_stream)(const SherpaOnnxOfflineStream *);
    void (*accept)(const SherpaOnnxOfflineStream *, int32_t, const float *, int32_t);
    void (*decode)(const SherpaOnnxOfflineRecognizer *, const SherpaOnnxOfflineStream *);
    const char *(*result)(const SherpaOnnxOfflineStream *);
    void (*free_result)(const char *);
} Parakeet;

void stt_parakeet_destroy(Parakeet *p) {
    if (!p) return;
    if (p->recognizer) p->destroy(p->recognizer);
    if (p->library) dlclose(p->library);
    free(p);
}

// POSIX guarantees dlsym function symbols have the callable pointer representation.
#define LOAD(field, name) do { \
    void *symbol = dlsym(p->library, name); \
    _Static_assert(sizeof(p->field) == sizeof(symbol), "unsupported dlsym ABI"); \
    if (!symbol) goto fail; \
    memcpy(&p->field, &symbol, sizeof(symbol)); \
} while (0)

Parakeet *stt_parakeet_create(const char *library, const char *encoder,
                            const char *decoder, const char *joiner,
                            const char *tokens, int32_t threads) {
    Parakeet *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->library = dlopen(library, RTLD_NOW | RTLD_LOCAL);
    if (!p->library) goto fail;
    LOAD(version, "SherpaOnnxGetVersionStr");
    const char *version = p->version();
    if (!version || (strcmp(version, "1.13.8") && strcmp(version, "v1.13.8"))) goto fail;
    LOAD(create, "SherpaOnnxCreateOfflineRecognizer");
    LOAD(destroy, "SherpaOnnxDestroyOfflineRecognizer");
    LOAD(stream, "SherpaOnnxCreateOfflineStream");
    LOAD(destroy_stream, "SherpaOnnxDestroyOfflineStream");
    LOAD(accept, "SherpaOnnxAcceptWaveformOffline");
    LOAD(decode, "SherpaOnnxDecodeOfflineStream");
    LOAD(result, "SherpaOnnxGetOfflineStreamResultAsJson");
    LOAD(free_result, "SherpaOnnxDestroyOfflineStreamResultJson");
    SherpaOnnxOfflineRecognizerConfig config = {0};
    config.feat_config.sample_rate = 16000;
    config.feat_config.feature_dim = 80;
    config.model_config.transducer.encoder = encoder;
    config.model_config.transducer.decoder = decoder;
    config.model_config.transducer.joiner = joiner;
    config.model_config.tokens = tokens;
    config.model_config.num_threads = threads;
    config.model_config.provider = "cpu";
    config.model_config.model_type = "nemo_transducer";
    config.decoding_method = "greedy_search";
    config.max_active_paths = 4;
    config.hotwords_score = 1.5f;
    p->recognizer = p->create(&config);
    if (!p->recognizer) goto fail;
    return p;
fail:
    stt_parakeet_destroy(p);
    return NULL;
}

const char *stt_parakeet_decode(Parakeet *p, const float *samples, int32_t count) {
    if (!p || !samples || count <= 0) return NULL;
    const SherpaOnnxOfflineStream *stream = p->stream(p->recognizer);
    if (!stream) return NULL;
    p->accept(stream, 16000, samples, count);
    p->decode(p->recognizer, stream);
    const char *json = p->result(stream);
    p->destroy_stream(stream);
    return json;
}

void stt_parakeet_free_result(Parakeet *p, const char *json) {
    if (p && json) p->free_result(json);
}
