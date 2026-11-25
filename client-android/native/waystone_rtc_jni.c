// Thin JNI bridge over the `waystone-rtc` C ABI (`waystone_rtc.h`).
//
// Keeps the Rust transport untouched and reuses the exact static library and
// CallbackGate contract shared with iOS.

#include "waystone_rtc.h"

#include <jni.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>

typedef struct {
    jobject callback_ref;
    jmethodID on_event_method;
} JniCallbackCtx;

typedef struct {
    WsEngine *engine;
    JniCallbackCtx *ctx;
} JniEngineWrapper;

static JavaVM *g_vm = NULL;
static pthread_key_t g_detach_key;
static pthread_once_t g_key_once = PTHREAD_ONCE_INIT;

static void detach_thread_on_exit(void *value) {
    (void)value;
    if (g_vm != NULL) {
        (*g_vm)->DetachCurrentThread(g_vm);
    }
}

static void init_detach_key(void) {
    pthread_key_create(&g_detach_key, detach_thread_on_exit);
}

JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *reserved) {
    (void)reserved;
    g_vm = vm;
    pthread_once(&g_key_once, init_detach_key);
    return JNI_VERSION_1_6;
}

static JNIEnv *acquire_env(void) {
    if (g_vm == NULL) {
        return NULL;
    }
    JNIEnv *env = NULL;
    jint status = (*g_vm)->GetEnv(g_vm, (void **)&env, JNI_VERSION_1_6);
    if (status == JNI_OK) {
        return env;
    }
    if (status == JNI_EDETACHED) {
#ifdef __ANDROID__
        if ((*g_vm)->AttachCurrentThread(g_vm, &env, NULL) != JNI_OK) {
            return NULL;
        }
#else
        if ((*g_vm)->AttachCurrentThread(g_vm, (void **)&env, NULL) != JNI_OK) {
            return NULL;
        }
#endif
        pthread_setspecific(g_detach_key, (void *)1);
        return env;
    }
    return NULL;
}

static void jni_event_callback(int32_t kind, const uint8_t *data, size_t len, void *raw_ctx) {
    JniCallbackCtx *ctx = (JniCallbackCtx *)raw_ctx;
    if (ctx == NULL || ctx->callback_ref == NULL) {
        return;
    }
    JNIEnv *env = acquire_env();
    if (env == NULL) {
        return;
    }
    jsize jlen = (jsize)len;
    jbyteArray arr = (*env)->NewByteArray(env, jlen);
    if (arr == NULL) {
        (*env)->ExceptionClear(env);
        return;
    }
    if (data != NULL && jlen > 0) {
        (*env)->SetByteArrayRegion(env, arr, 0, jlen, (const jbyte *)data);
    }
    (*env)->CallVoidMethod(env, ctx->callback_ref, ctx->on_event_method, (jint)kind, arr);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
    (*env)->DeleteLocalRef(env, arr);
}

static void throw_runtime(JNIEnv *env, const char *message) {
    jclass ex = (*env)->FindClass(env, "java/lang/RuntimeException");
    if (ex != NULL) {
        (*env)->ThrowNew(env, ex, message != NULL ? message : "transport error");
        (*env)->DeleteLocalRef(env, ex);
    }
}

JNIEXPORT jlong JNICALL Java_dev_waystone_app_stream_Transport_nativeStart(
    JNIEnv *env, jobject thiz, jstring config_json, jobject callback) {
    (void)thiz;
    if (config_json == NULL || callback == NULL) {
        throw_runtime(env, "config and callback are required");
        return 0;
    }

    jclass cb_class = (*env)->GetObjectClass(env, callback);
    if (cb_class == NULL) {
        return 0;
    }
    jmethodID on_event = (*env)->GetMethodID(env, cb_class, "onNativeEvent", "(I[B)V");
    (*env)->DeleteLocalRef(env, cb_class);
    if (on_event == NULL) {
        return 0;
    }

    JniCallbackCtx *ctx = (JniCallbackCtx *)calloc(1, sizeof(JniCallbackCtx));
    JniEngineWrapper *wrapper = (JniEngineWrapper *)calloc(1, sizeof(JniEngineWrapper));
    if (ctx == NULL || wrapper == NULL) {
        free(ctx);
        free(wrapper);
        throw_runtime(env, "out of memory");
        return 0;
    }

    ctx->callback_ref = (*env)->NewGlobalRef(env, callback);
    ctx->on_event_method = on_event;
    if (ctx->callback_ref == NULL) {
        free(ctx);
        free(wrapper);
        throw_runtime(env, "cannot retain callback");
        return 0;
    }

    const char *json_chars = (*env)->GetStringUTFChars(env, config_json, NULL);
    if (json_chars == NULL) {
        (*env)->DeleteGlobalRef(env, ctx->callback_ref);
        free(ctx);
        free(wrapper);
        return 0;
    }

    char error_buf[512] = {0};
    WsEngine *engine = ws_engine_start(
        json_chars, jni_event_callback, ctx, error_buf, sizeof(error_buf));
    (*env)->ReleaseStringUTFChars(env, config_json, json_chars);

    if (engine == NULL) {
        (*env)->DeleteGlobalRef(env, ctx->callback_ref);
        free(ctx);
        free(wrapper);
        throw_runtime(env, error_buf[0] != '\0' ? error_buf : "ws_engine_start failed");
        return 0;
    }

    wrapper->engine = engine;
    wrapper->ctx = ctx;
    return (jlong)(uintptr_t)wrapper;
}

JNIEXPORT void JNICALL Java_dev_waystone_app_stream_Transport_nativeStop(
    JNIEnv *env, jobject thiz, jlong handle) {
    (void)thiz;
    if (handle == 0) {
        return;
    }
    JniEngineWrapper *wrapper = (JniEngineWrapper *)(uintptr_t)handle;
    // ws_engine_stop closes CallbackGate before returning, so no callback can
    // race with freeing `ctx` afterwards.
    ws_engine_stop(wrapper->engine);
    if (wrapper->ctx != NULL) {
        if (wrapper->ctx->callback_ref != NULL) {
            (*env)->DeleteGlobalRef(env, wrapper->ctx->callback_ref);
        }
        free(wrapper->ctx);
    }
    free(wrapper);
}

JNIEXPORT void JNICALL Java_dev_waystone_app_stream_Transport_nativeAcceptAnswer(
    JNIEnv *env, jobject thiz, jlong handle, jstring sdp) {
    (void)thiz;
    if (handle == 0 || sdp == NULL) {
        return;
    }
    JniEngineWrapper *wrapper = (JniEngineWrapper *)(uintptr_t)handle;
    const char *chars = (*env)->GetStringUTFChars(env, sdp, NULL);
    if (chars != NULL) {
        ws_engine_accept_answer(wrapper->engine, chars);
        (*env)->ReleaseStringUTFChars(env, sdp, chars);
    }
}

JNIEXPORT void JNICALL Java_dev_waystone_app_stream_Transport_nativeAddCandidate(
    JNIEnv *env, jobject thiz, jlong handle, jstring candidate) {
    (void)thiz;
    if (handle == 0 || candidate == NULL) {
        return;
    }
    JniEngineWrapper *wrapper = (JniEngineWrapper *)(uintptr_t)handle;
    const char *chars = (*env)->GetStringUTFChars(env, candidate, NULL);
    if (chars != NULL) {
        ws_engine_add_candidate(wrapper->engine, chars);
        (*env)->ReleaseStringUTFChars(env, candidate, chars);
    }
}

JNIEXPORT void JNICALL Java_dev_waystone_app_stream_Transport_nativeSendInput(
    JNIEnv *env, jobject thiz, jlong handle, jboolean reliable, jbyteArray data) {
    (void)thiz;
    if (handle == 0 || data == NULL) {
        return;
    }
    JniEngineWrapper *wrapper = (JniEngineWrapper *)(uintptr_t)handle;
    jsize len = (*env)->GetArrayLength(env, data);
    if (len <= 0) {
        return;
    }
    jbyte *bytes = (*env)->GetByteArrayElements(env, data, NULL);
    if (bytes != NULL) {
        ws_engine_send_input(
            wrapper->engine, reliable == JNI_TRUE, (const uint8_t *)bytes, (size_t)len);
        (*env)->ReleaseByteArrayElements(env, data, bytes, JNI_ABORT);
    }
}
