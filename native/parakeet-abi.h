// Copyright (c) 2023 Xiaomi Corporation
// SPDX-License-Identifier: Apache-2.0
// Offline C ABI declarations extracted from sherpa-onnx v1.13.8 c-api.h.
// Unrelated declarations and comments omitted; field order is unchanged.
// Only used with the pinned 1.13.8 runtime, checked before creating a recognizer.
#pragma once
#include <stdint.h>

typedef struct { int32_t sample_rate, feature_dim; } SherpaOnnxFeatureConfig;
typedef struct { const char *dict_dir, *lexicon, *rule_fsts; } SherpaOnnxHomophoneReplacerConfig;
typedef struct { const char *encoder, *decoder, *joiner; } SherpaOnnxOfflineTransducerModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineParaformerModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineNemoEncDecCtcModelConfig;
typedef struct {
    const char *encoder, *decoder, *language, *task;
    int32_t tail_paddings, enable_token_timestamps, enable_segment_timestamps;
} SherpaOnnxOfflineWhisperModelConfig;
typedef struct { const char *encoder, *decoder, *src_lang, *tgt_lang; int32_t use_pnc; } SherpaOnnxOfflineCanaryModelConfig;
typedef struct { const char *encoder, *decoder, *language; int32_t use_punct, use_itn; } SherpaOnnxOfflineCohereTranscribeModelConfig;
typedef struct { const char *encoder, *decoder; } SherpaOnnxOfflineFireRedAsrModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineFireRedAsrCtcModelConfig;
typedef struct { const char *preprocessor, *encoder, *uncached_decoder, *cached_decoder, *merged_decoder; } SherpaOnnxOfflineMoonshineModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineTdnnModelConfig;
typedef struct { const char *model; float scale; } SherpaOnnxOfflineLMConfig;
typedef struct { const char *model, *language; int32_t use_itn; } SherpaOnnxOfflineSenseVoiceModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineDolphinModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineZipformerCtcModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineWenetCtcModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineOmnilingualAsrCtcModelConfig;
typedef struct {
    const char *encoder_adaptor, *llm, *embedding, *tokenizer, *system_prompt, *user_prompt;
    int32_t max_new_tokens;
    float temperature, top_p;
    int32_t seed;
    const char *language;
    int32_t itn;
    const char *hotwords;
} SherpaOnnxOfflineFunASRNanoModelConfig;
typedef struct {
    const char *conv_frontend, *encoder, *decoder, *tokenizer;
    int32_t max_total_len, max_new_tokens;
    float temperature, top_p;
    int32_t seed;
    const char *hotwords;
} SherpaOnnxOfflineQwen3ASRModelConfig;
typedef struct { const char *model; } SherpaOnnxOfflineMedAsrCtcModelConfig;
typedef struct {
    SherpaOnnxOfflineTransducerModelConfig transducer;
    SherpaOnnxOfflineParaformerModelConfig paraformer;
    SherpaOnnxOfflineNemoEncDecCtcModelConfig nemo_ctc;
    SherpaOnnxOfflineWhisperModelConfig whisper;
    SherpaOnnxOfflineTdnnModelConfig tdnn;
    const char *tokens;
    int32_t num_threads, debug;
    const char *provider, *model_type, *modeling_unit, *bpe_vocab, *telespeech_ctc;
    SherpaOnnxOfflineSenseVoiceModelConfig sense_voice;
    SherpaOnnxOfflineMoonshineModelConfig moonshine;
    SherpaOnnxOfflineFireRedAsrModelConfig fire_red_asr;
    SherpaOnnxOfflineDolphinModelConfig dolphin;
    SherpaOnnxOfflineZipformerCtcModelConfig zipformer_ctc;
    SherpaOnnxOfflineCanaryModelConfig canary;
    SherpaOnnxOfflineWenetCtcModelConfig wenet_ctc;
    SherpaOnnxOfflineOmnilingualAsrCtcModelConfig omnilingual;
    SherpaOnnxOfflineMedAsrCtcModelConfig medasr;
    SherpaOnnxOfflineFunASRNanoModelConfig funasr_nano;
    SherpaOnnxOfflineFireRedAsrCtcModelConfig fire_red_asr_ctc;
    SherpaOnnxOfflineQwen3ASRModelConfig qwen3_asr;
    SherpaOnnxOfflineCohereTranscribeModelConfig cohere_transcribe;
} SherpaOnnxOfflineModelConfig;
typedef struct {
    SherpaOnnxFeatureConfig feat_config;
    SherpaOnnxOfflineModelConfig model_config;
    SherpaOnnxOfflineLMConfig lm_config;
    const char *decoding_method;
    int32_t max_active_paths;
    const char *hotwords_file;
    float hotwords_score;
    const char *rule_fsts, *rule_fars;
    float blank_penalty;
    SherpaOnnxHomophoneReplacerConfig hr;
} SherpaOnnxOfflineRecognizerConfig;
typedef struct SherpaOnnxOfflineRecognizer SherpaOnnxOfflineRecognizer;
typedef struct SherpaOnnxOfflineStream SherpaOnnxOfflineStream;
