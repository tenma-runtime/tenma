/*
 * tenma native plugins, ABI version 1.
 *
 * A plugin is a shared library that exports one symbol:
 *
 *     const TenmaPlugin *tenma_plugin_init(const TenmaApi *api, uint32_t abi);
 *
 * tenma loads it with dlopen before any worker starts (`tenma --plugin ./libname.so`, or `plugins` in
 * tenma.json) and calls it once; the plugin keeps `api` for the life of the process and returns its
 * table, or NULL if `abi` is not the version it was built for. Memory crosses the boundary only as
 * pointer and length, valid for the duration of the call: whoever needs it longer copies it.
 */
#ifndef TENMA_PLUGIN_H
#define TENMA_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define TENMA_PLUGIN_ABI 1

/* What api->send reports. */
enum tenma_send_status {
    TENMA_SENT = 0,
    /* The connection's queue is full: try again later, or drop the message. */
    TENMA_FULL = 1,
    /* The connection is closed or was never adopted. */
    TENMA_CLOSED = 2,
};

enum tenma_log_level {
    TENMA_LOG_ERROR = 0,
    TENMA_LOG_WARNING = 1,
    TENMA_LOG_INFO = 2,
};

/* A buffer tenma owns, filled by api->reply during a synchronous call. */
typedef struct TenmaReply TenmaReply;
/* The token of an asynchronous call: completed exactly once with api->complete. */
typedef struct TenmaToken TenmaToken;

/* tenma to plugin. Every function may be called from any thread. */
typedef struct {
    /* Queues a message on a connection the plugin adopted; never waits. `binary` 0 sends a text frame. */
    int (*send)(uint64_t conn, const uint8_t *ptr, size_t len, int binary);
    /* Bytes queued on the connection and not yet written, for back pressure; 0 for a closed one. */
    size_t (*buffered)(uint64_t conn);
    /* Closes the connection with a WebSocket close code (1000 for normal). */
    void (*close)(uint64_t conn, uint16_t code);
    /* Delivers a message to every PHP subscription of the channel in this process (Messaging\Channel). */
    void (*post)(const char *channel, size_t channel_len, const uint8_t *ptr, size_t len);
    /* Finishes a call_async: the waiting fiber gets the bytes, or throws `error` != 0 with them as the message. */
    void (*complete)(TenmaToken *token, const uint8_t *ptr, size_t len, int error);
    /* Sets the result of a synchronous call. Without it the result is empty. */
    void (*reply)(TenmaReply *out, const uint8_t *ptr, size_t len);
    /* Writes a line to tenma's log (stderr). */
    void (*log)(int level, const char *ptr, size_t len);
} TenmaApi;

/* Plugin to tenma. Functions that may be NULL are optional. */
typedef struct {
    /* For errors and the name PHP uses: Tenma\Plugin::call('name', ...). */
    const char *name;
    /* A synchronous call on the worker thread: for quick things. 0 on success; otherwise the reply is the error message. */
    int (*call)(const char *fn, size_t fn_len, const uint8_t *in, size_t in_len, TenmaReply *out);
    /* Starts an asynchronous call on the worker thread and returns at once; api->complete(token, ...) later, from any thread. */
    void (*call_async)(const char *fn, size_t fn_len, const uint8_t *in, size_t in_len, TenmaToken *token);
    /* A PHP handler handed this WebSocket over; the plugin may send on it from now on. May be NULL: adopt() then fails. */
    void (*adopt)(uint64_t conn);
    /* An incoming message on an adopted connection, on tenma's I/O thread: return quickly. */
    void (*message)(uint64_t conn, const uint8_t *ptr, size_t len, int binary);
    /* The client or tenma closed an adopted connection. */
    void (*closed)(uint64_t conn);
    /* Once, when tenma stops, after PHP has run its stop handlers. May be NULL. */
    void (*shutdown)(void);
} TenmaPlugin;

#ifdef __cplusplus
}
#endif

#endif
