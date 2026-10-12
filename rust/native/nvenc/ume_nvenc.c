// H.264 encoding of system-memory RGBA frames through the NVIDIA driver's own
// encoder (libnvidia-encode) on a CUDA context. Both libraries are opened at
// run time, so the tool starts on machines without an NVIDIA driver.
//
// Tuned for interactive streaming: no B-frames, no lookahead, constant
// bitrate, an infinite GOP and IDR frames only when the caller asks. Every
// call returns the complete access unit of the frame it was given.
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "nvEncodeAPI.h"

typedef int (*cu_init)(unsigned int);
typedef int (*cu_device_get)(int *, int);
typedef int (*cu_ctx_create)(void **, unsigned int, int);
typedef int (*cu_ctx_push)(void *);
typedef int (*cu_ctx_pop)(void **);
typedef int (*cu_ctx_destroy)(void *);

typedef struct ume_nvenc {
    cu_ctx_push push;
    cu_ctx_pop pop;
    cu_ctx_destroy destroy;
    void *context;
    NV_ENCODE_API_FUNCTION_LIST api;
    void *encoder;
    NV_ENC_INPUT_PTR input;
    NV_ENC_OUTPUT_PTR output;
    uint32_t width, height;
} ume_nvenc;

static void report(char *error, size_t capacity, const char *format, ...) {
    if (error == NULL || capacity == 0) return;
    va_list arguments;
    va_start(arguments, format);
    vsnprintf(error, capacity, format, arguments);
    va_end(arguments);
}

static void *symbol(void *library, const char *name, char *error, size_t capacity) {
    void *found = dlsym(library, name);
    if (found == NULL) report(error, capacity, "%s is missing from the NVIDIA driver library", name);
    return found;
}

static void describe(ume_nvenc *state, const char *step, NVENCSTATUS status, char *error, size_t capacity) {
    const char *detail = state->encoder != NULL && state->api.nvEncGetLastErrorString != NULL
        ? state->api.nvEncGetLastErrorString(state->encoder) : NULL;
    report(error, capacity, "%s failed with NVENC status %d%s%s", step, (int)status,
        detail != NULL && detail[0] != 0 ? ": " : "", detail != NULL ? detail : "");
}

void ume_nvenc_close(ume_nvenc *state) {
    if (state == NULL) return;
    void *previous = NULL;
    if (state->context != NULL) state->push(state->context);
    if (state->encoder != NULL) {
        if (state->input != NULL) state->api.nvEncDestroyInputBuffer(state->encoder, state->input);
        if (state->output != NULL) state->api.nvEncDestroyBitstreamBuffer(state->encoder, state->output);
        state->api.nvEncDestroyEncoder(state->encoder);
    }
    if (state->context != NULL) {
        state->pop(&previous);
        state->destroy(state->context);
    }
    free(state);
}

// Returns 0 and the encoder, or -1 with a message. `device` is the CUDA
// ordinal of the GPU.
int ume_nvenc_open(uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate, int device,
    ume_nvenc **result, char *error, size_t capacity) {
    *result = NULL;
    if (width < 2 || height < 2 || (width & 1) != 0 || (height & 1) != 0 || fps < 1 || bitrate < 1) {
        report(error, capacity, "frame size must be even and the rate positive");
        return -1;
    }
    void *cuda = dlopen("libcuda.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (cuda == NULL) { report(error, capacity, "libcuda.so.1 is unavailable: %s", dlerror()); return -1; }
    void *library = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (library == NULL) { report(error, capacity, "libnvidia-encode.so.1 is unavailable: %s", dlerror()); return -1; }
    cu_init init = (cu_init)symbol(cuda, "cuInit", error, capacity);
    cu_device_get device_get = (cu_device_get)symbol(cuda, "cuDeviceGet", error, capacity);
    cu_ctx_create create = (cu_ctx_create)symbol(cuda, "cuCtxCreate_v2", error, capacity);
    cu_ctx_push push = (cu_ctx_push)symbol(cuda, "cuCtxPushCurrent_v2", error, capacity);
    cu_ctx_pop pop = (cu_ctx_pop)symbol(cuda, "cuCtxPopCurrent_v2", error, capacity);
    cu_ctx_destroy destroy = (cu_ctx_destroy)symbol(cuda, "cuCtxDestroy_v2", error, capacity);
    NVENCSTATUS (NVENCAPI *maximum)(uint32_t *) = symbol(library, "NvEncodeAPIGetMaxSupportedVersion", error, capacity);
    NVENCSTATUS (NVENCAPI *instance)(NV_ENCODE_API_FUNCTION_LIST *) = symbol(library, "NvEncodeAPICreateInstance", error, capacity);
    if (!init || !device_get || !create || !push || !pop || !destroy || !maximum || !instance) return -1;

    uint32_t supported = 0;
    if (maximum(&supported) != NV_ENC_SUCCESS || supported < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        report(error, capacity, "the NVIDIA driver supports NVENC API %u.%u, older than the required %d.%d",
            supported >> 4, supported & 15, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
        return -1;
    }
    int status = init(0), ordinal = 0;
    if (status != 0) { report(error, capacity, "cuInit failed with CUDA status %d", status); return -1; }
    if ((status = device_get(&ordinal, device)) != 0) { report(error, capacity, "CUDA device %d is unavailable (status %d)", device, status); return -1; }

    ume_nvenc *state = calloc(1, sizeof(ume_nvenc));
    if (state == NULL) { report(error, capacity, "out of memory"); return -1; }
    state->push = push; state->pop = pop; state->destroy = destroy;
    state->width = width; state->height = height;
    // The new context is current on this thread until it is popped below.
    if ((status = create(&state->context, 0, ordinal)) != 0) {
        report(error, capacity, "cuCtxCreate failed with CUDA status %d", status);
        free(state);
        return -1;
    }
    void *previous = NULL;
    NVENCSTATUS code;
    const char *step;
    state->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    step = "NvEncodeAPICreateInstance";
    if ((code = instance(&state->api)) != NV_ENC_SUCCESS) goto failed;

    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session = {0};
    session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    session.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    session.device = state->context;
    session.apiVersion = NVENCAPI_VERSION;
    step = "opening an encode session";
    if ((code = state->api.nvEncOpenEncodeSessionEx(&session, &state->encoder)) != NV_ENC_SUCCESS) {
        state->encoder = NULL;
        goto failed;
    }

    NV_ENC_PRESET_CONFIG preset = {0};
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    step = "reading the low-latency preset";
    if ((code = state->api.nvEncGetEncodePresetConfigEx(state->encoder, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P3_GUID,
            NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset)) != NV_ENC_SUCCESS) goto failed;

    NV_ENC_CONFIG config = preset.presetCfg;
    config.version = NV_ENC_CONFIG_VER;
    config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
    config.gopLength = NVENC_INFINITE_GOPLENGTH;
    config.frameIntervalP = 1;
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate;
    // Two frames of buffer: an IDR frame keeps its quality without adding delay.
    config.rcParams.vbvBufferSize = (uint32_t)((uint64_t)bitrate * 2 / fps);
    config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
    config.rcParams.zeroReorderDelay = 1;
    config.rcParams.enableAQ = 1;
    NV_ENC_CONFIG_H264 *h264 = &config.encodeCodecConfig.h264Config;
    h264->idrPeriod = NVENC_INFINITE_GOPLENGTH;
    h264->repeatSPSPPS = 1;
    h264->h264VUIParameters.videoSignalTypePresentFlag = 1;
    h264->h264VUIParameters.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    h264->h264VUIParameters.videoFullRangeFlag = 0;
    h264->h264VUIParameters.colourDescriptionPresentFlag = 1;
    h264->h264VUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    h264->h264VUIParameters.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    h264->h264VUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;

    NV_ENC_INITIALIZE_PARAMS initialize = {0};
    initialize.version = NV_ENC_INITIALIZE_PARAMS_VER;
    initialize.encodeGUID = NV_ENC_CODEC_H264_GUID;
    initialize.presetGUID = NV_ENC_PRESET_P3_GUID;
    initialize.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    initialize.encodeWidth = width;
    initialize.encodeHeight = height;
    initialize.darWidth = width;
    initialize.darHeight = height;
    initialize.frameRateNum = fps;
    initialize.frameRateDen = 1;
    initialize.enablePTD = 1;
    initialize.encodeConfig = &config;
    step = "initializing the encoder";
    if ((code = state->api.nvEncInitializeEncoder(state->encoder, &initialize)) != NV_ENC_SUCCESS) goto failed;

    NV_ENC_CREATE_INPUT_BUFFER input = {0};
    input.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
    input.width = width;
    input.height = height;
    input.bufferFmt = NV_ENC_BUFFER_FORMAT_ABGR;
    step = "allocating the input surface";
    if ((code = state->api.nvEncCreateInputBuffer(state->encoder, &input)) != NV_ENC_SUCCESS) goto failed;
    state->input = input.inputBuffer;

    NV_ENC_CREATE_BITSTREAM_BUFFER output = {0};
    output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    step = "allocating the bitstream buffer";
    if ((code = state->api.nvEncCreateBitstreamBuffer(state->encoder, &output)) != NV_ENC_SUCCESS) goto failed;
    state->output = output.bitstreamBuffer;

    state->pop(&previous);
    *result = state;
    return 0;

failed:
    describe(state, step, code, error, capacity);
    state->pop(&previous);
    ume_nvenc_close(state);
    return -1;
}

// Encodes one frame of tightly packed R,G,B,A rows, top row first. Returns the
// size of the Annex-B access unit copied into `output`, or -1 with a message.
int64_t ume_nvenc_encode(ume_nvenc *state, const uint8_t *pixels, int force_idr, uint64_t timestamp,
    uint8_t *output, size_t output_capacity, int *keyframe, char *error, size_t capacity) {
    void *previous = NULL;
    NVENCSTATUS code;
    const char *step;
    int64_t size = -1;
    state->push(state->context);

    NV_ENC_LOCK_INPUT_BUFFER surface = {0};
    surface.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
    surface.inputBuffer = state->input;
    step = "locking the input surface";
    if ((code = state->api.nvEncLockInputBuffer(state->encoder, &surface)) != NV_ENC_SUCCESS) goto failed;
    size_t row = (size_t)state->width * 4;
    for (uint32_t y = 0; y < state->height; y++)
        memcpy((uint8_t *)surface.bufferDataPtr + (size_t)y * surface.pitch, pixels + (size_t)y * row, row);
    step = "unlocking the input surface";
    if ((code = state->api.nvEncUnlockInputBuffer(state->encoder, state->input)) != NV_ENC_SUCCESS) goto failed;

    NV_ENC_PIC_PARAMS picture = {0};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = state->width;
    picture.inputHeight = state->height;
    picture.inputPitch = surface.pitch;
    picture.encodePicFlags = force_idr ? NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS : 0;
    picture.inputBuffer = state->input;
    picture.outputBitstream = state->output;
    picture.bufferFmt = NV_ENC_BUFFER_FORMAT_ABGR;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.inputTimeStamp = timestamp;
    step = "encoding a frame";
    if ((code = state->api.nvEncEncodePicture(state->encoder, &picture)) != NV_ENC_SUCCESS) goto failed;

    NV_ENC_LOCK_BITSTREAM bitstream = {0};
    bitstream.version = NV_ENC_LOCK_BITSTREAM_VER;
    bitstream.outputBitstream = state->output;
    step = "reading the encoded frame";
    if ((code = state->api.nvEncLockBitstream(state->encoder, &bitstream)) != NV_ENC_SUCCESS) goto failed;
    if (bitstream.bitstreamSizeInBytes <= output_capacity) {
        memcpy(output, bitstream.bitstreamBufferPtr, bitstream.bitstreamSizeInBytes);
        size = bitstream.bitstreamSizeInBytes;
        *keyframe = bitstream.pictureType == NV_ENC_PIC_TYPE_IDR;
    } else {
        report(error, capacity, "the encoded frame of %u bytes exceeds the output buffer", bitstream.bitstreamSizeInBytes);
    }
    state->api.nvEncUnlockBitstream(state->encoder, state->output);
    state->pop(&previous);
    return size;

failed:
    describe(state, step, code, error, capacity);
    state->pop(&previous);
    return -1;
}
