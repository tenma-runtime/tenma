// The C layer between Rust and PHP. Only this file touches the Zend API: embed PHP,
// provide functions to PHP code, call handlers, convert PHP values into plain C values.
// Rust never sees a zval. Every call into PHP runs inside zend_try, so a fatal error
// (zend_bailout, a longjmp) never jumps through Rust code.

#include <sapi/embed/php_embed.h>
#include <Zend/zend_exceptions.h>
#include <Zend/zend_extensions.h>
#include <Zend/zend_fibers.h>
#include <Zend/zend_enum.h>
#include <Zend/zend_lazy_objects.h>
#include <Zend/zend_observer.h>
#include <Zend/zend_signal.h>
#include <main/rfc1867.h>
#include <main/php_ini_builder.h>
#include <Zend/zend_smart_str.h>
#include <ext/filter/php_filter.h>
#include <ext/session/php_session.h>
#include <ext/standard/basic_functions.h>
#include <ext/spl/spl_exceptions.h>
#include <main/fopen_wrappers.h>
#include <main/php_network.h>

ZEND_EXTERN_MODULE_GLOBALS(filter)
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <sys/socket.h>
#include <unistd.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    const char *ptr;
    size_t len;
} tenma_str;

typedef struct {
    tenma_str name;
    tenma_str value;
} tenma_header;

typedef struct {
    tenma_str method;
    tenma_str uri;
    tenma_str body;
    const tenma_header *headers;
    size_t header_count;
    /** Both ends of the connection, as text ("127.0.0.1") and port. */
    tenma_str remote_addr;
    uint16_t remote_port;
    tenma_str server_addr;
    uint16_t server_port;
    /** The connection came over TLS (--tls-cert). */
    bool https;
    /** "HTTP/1.1", "HTTP/2" or "HTTP/1.0". */
    tenma_str protocol;
} tenma_request;

/** Owned by Rust after tenma_handle; free with tenma_response_free. */
typedef struct {
    uint16_t status;
    char *body;
    size_t body_len;
    /** Header lines as with header(): "Name: Value\0Name: Value\0" … */
    char *headers;
    size_t headers_len;
} tenma_response;

/** Rust stores port and handler; returns 0 if the port is invalid. */
extern int tenma_rs_listen(zend_long port, uint32_t handler, const char *mounts, size_t mounts_len);
/** Hands Rust the response to request rid. Rust frees it with tenma_response_free. */
extern void tenma_rs_respond(uint64_t rid, tenma_response *response);
/** Async ops: Rust starts them and returns an op ID; Rust reports completion via tenma_complete_*. */
extern uint64_t tenma_rs_sleep(zend_long milliseconds);
/** Streaming responses and WebSockets, see src/streams.rs. */
extern int tenma_rs_stream_start(uint64_t rid, uint16_t status, const char *headers, size_t headers_len);
extern int64_t tenma_rs_stream_write(uint64_t rid, const char *chunk, size_t len);
extern void tenma_rs_stream_end(uint64_t rid, int finished);
extern int tenma_rs_ws_accept(uint64_t rid);
extern int64_t tenma_rs_ws_send(uint64_t rid, const char *text, size_t len);
extern uint64_t tenma_rs_ws_receive(uint64_t rid);
extern void tenma_rs_ws_close(uint64_t rid, int finished);
/** Databases, see src/db.rs. */
extern void tenma_rs_db_abandon(uint64_t rid);
extern void tenma_rs_op_cancel(uint64_t op);
extern uint64_t tenma_rs_pubsub(const char *op, size_t op_len, uint64_t id, const char *channel, size_t channel_len,
    const char *message, size_t message_len);
/** File system, see src/fs.rs. */
extern uint64_t tenma_rs_fs(const char *op, size_t op_len, const char *path, size_t path_len,
    const char *second, size_t second_len, bool flag, const char *data, size_t data_len);
/** exit() outside a request: tenma exits with this status. */
extern void tenma_rs_exit(int status);
/** tenma --watch: a file PHP loaded, see tenma_report_included. */
extern void tenma_rs_included(const char *path, size_t len);
/** tenma --watch: the length of the path of the change behind this run, copied into buffer if it fits; SIZE_MAX for none. */
extern size_t tenma_rs_reloaded_by(char *buffer, size_t capacity);
/** tenma --inspect: a statement is about to run, see inspect_statement. */
extern void tenma_rs_statement(const char *file, size_t file_len, uint32_t line, uintptr_t frame, uintptr_t fiber);
/** tenma --inspect: a function is about to return, at its closing brace on `line`. */
extern void tenma_rs_return(const char *file, size_t file_len, uint32_t line, uintptr_t frame, uintptr_t fiber);
/** tenma --inspect: PHP compiled a function (or a file's top level, `main`) with statements on these lines. */
extern void tenma_rs_compiled(const char *file, size_t file_len, uint32_t start, uint32_t end, bool main, const uint32_t *lines, size_t count);
extern uint64_t tenma_rs_dev(const char *op, size_t op_len, const char *arg, size_t arg_len);
extern uint64_t tenma_rs_db(const char *op, size_t op_len, const char *target, size_t target_len,
    const char *sql, size_t sql_len, const char *params, size_t params_len);
extern uint64_t tenma_rs_fetch(const char *method, size_t method_len, const char *url, size_t url_len,
    const char *headers, size_t headers_len, const char *body, size_t body_len, uint64_t timeout_ms);

// Each worker thread has its own PHP interpreter, and so its own handlers
#define MAX_HANDLERS 64
static _Thread_local zval handlers[MAX_HANDLERS];
static _Thread_local uint32_t handler_count = 0;

/** The request PHP last answered with tenma_respond. */
static _Thread_local uint64_t responded_rid = 0;
/** exit() or die() ran in this thread since tenma_execute began (see tenma_exit). */
static _Thread_local int script_exited = 0;
/** A classic request is running (tenma_classic_begin); its shutdown functions and response are pending. */
static _Thread_local int classic_open = 0;
/** The shutdown functions from script startup, set aside during a classic request. */
static _Thread_local HashTable *outer_shutdown_functions = NULL;
/**
 * The output of the running classic request. Tenma collects it where PHP writes (ub_write),
 * not in an ob_start() buffer: frameworks that flush all output
 * buffers when sending, and the response would otherwise have ended up on the console.
 */
static _Thread_local smart_str classic_output = {0};
/** Where the embed SAPI writes when no classic request is running: stdout. */
static size_t (*embed_ub_write)(const char *str, size_t length) = NULL;

/** tenma --inspect, set before tenma_start. */
static int inspect_mode = 0;

/** tenma --inspect: output also goes to the editor's debug console; 1 for the error log. */
extern void tenma_rs_output(const char *text, size_t len, int log);

extern int tenma_rs_output_muted(void);

static size_t tenma_ub_write(const char *str, size_t length) {
    if (inspect_mode && tenma_rs_output_muted()) {
        return length;
    }
    if (classic_open) {
        smart_str_appendl(&classic_output, str, length);
        return length;
    }
    if (inspect_mode) {
        tenma_rs_output(str, length, 0);
    }
    return embed_ub_write(str, length);
}

static void (*embed_log_message)(const char *message, int syslog_type) = NULL;

static void tenma_log_message(const char *message, int syslog_type) {
    if (tenma_rs_output_muted()) {
        return;
    }
    tenma_rs_output(message, strlen(message), 1);
    embed_log_message(message, syslog_type);
}

/** ini values changed before the classic request: name → value. What the request changes is restored afterwards. */
static _Thread_local HashTable *ini_snapshot = NULL;

static char *resolve_path(zend_string *path);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_listen, 0, 2, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, port, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, handler, IS_CALLABLE, 0)
    ZEND_ARG_TYPE_INFO(0, mounts, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

/**
 * tenma_listen(int $port, callable $handler, array $mounts = []): void. The handler receives
 * (array $request, int $rid) and responds via tenma_respond. $mounts: URL prefix → directory or
 * file; Rust serves whatever lies below it.
 */
PHP_FUNCTION(tenma_listen) {
    zend_long port;
    zval *handler;
    HashTable *mounts = NULL;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_LONG(port)
        Z_PARAM_ZVAL(handler)
        Z_PARAM_OPTIONAL
        Z_PARAM_ARRAY_HT(mounts)
    ZEND_PARSE_PARAMETERS_END();

    if (!zend_is_callable(handler, 0, NULL)) {
        zend_argument_type_error(2, "must be a valid callback");
        RETURN_THROWS();
    }
    if (handler_count == MAX_HANDLERS) {
        zend_throw_error(NULL, "Too many servers");
        RETURN_THROWS();
    }
    // Prefix and path alternating, each terminated by a null byte. Paths as with PHP's file functions:
    // relative to the working directory, open_basedir applies
    smart_str packed = {0};
    if (mounts) {
        zend_string *prefix;
        zval *target;
        ZEND_HASH_FOREACH_STR_KEY_VAL(mounts, prefix, target) {
            if (!prefix || Z_TYPE_P(target) != IS_STRING || memchr(ZSTR_VAL(prefix), 0, ZSTR_LEN(prefix))
                || memchr(Z_STRVAL_P(target), 0, Z_STRLEN_P(target))) {
                smart_str_free(&packed);
                zend_argument_type_error(3, "must map URL prefixes to paths");
                RETURN_THROWS();
            }
            char *path = resolve_path(Z_STR_P(target));
            if (!path) {
                smart_str_free(&packed);
                RETURN_THROWS();
            }
            smart_str_append(&packed, prefix);
            smart_str_appendc(&packed, '\0');
            smart_str_appends(&packed, path);
            smart_str_appendc(&packed, '\0');
            efree(path);
        } ZEND_HASH_FOREACH_END();
    }
    int registered = tenma_rs_listen(port, handler_count, packed.s ? ZSTR_VAL(packed.s) : "", packed.s ? ZSTR_LEN(packed.s) : 0);
    smart_str_free(&packed);
    if (registered == 0) {
        zend_argument_value_error(1, "must be between 1 and 65535");
        RETURN_THROWS();
    }
    if (registered < 0) {
        zend_argument_value_error(3, "must map to existing directories or files");
        RETURN_THROWS();
    }
    ZVAL_COPY(&handlers[handler_count++], handler);
}

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_respond, 0, 2, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, response, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_sleep, 0, 1, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, milliseconds, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_fetch, 0, 5, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, method, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, url, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, headers, IS_ARRAY, 0)
    ZEND_ARG_TYPE_INFO(0, body, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, timeout_ms, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_set_input, 0, 1, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, body, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_parse_multipart, 0, 2, IS_ARRAY, 0)
    ZEND_ARG_TYPE_INFO(0, content_type, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, body, IS_STRING, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_respond);
PHP_FUNCTION(tenma_op_sleep);
PHP_FUNCTION(tenma_op_fetch);
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_forget_session, 0, 0, IS_VOID, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_set_input);
PHP_FUNCTION(tenma_parse_multipart);
PHP_FUNCTION(tenma_forget_session);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_stream_start, 0, 3, _IS_BOOL, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, status, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, headers, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_rid_string_long, 0, 2, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, data, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_rid_void, 0, 1, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_rid_finished, 0, 2, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, finished, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_rid_bool, 0, 1, _IS_BOOL, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_rid_long, 0, 1, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_stream_start);
PHP_FUNCTION(tenma_stream_write);
PHP_FUNCTION(tenma_stream_end);
PHP_FUNCTION(tenma_ws_accept);
PHP_FUNCTION(tenma_ws_send);
PHP_FUNCTION(tenma_ws_receive);
PHP_FUNCTION(tenma_ws_close);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_db, 0, 4, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, op, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, target, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, sql, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, params, IS_STRING, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_op_db);
PHP_FUNCTION(tenma_db_abandon);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_parse_cookies, 0, 2, IS_ARRAY, 0)
    ZEND_ARG_TYPE_INFO(0, header, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, global, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_parse_cookies);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_cancel, 0, 1, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, op, IS_LONG, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_op_cancel);
PHP_FUNCTION(tenma_exit_process);
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_fs, 0, 5, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, op, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, path, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, second, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, flag, _IS_BOOL, 0)
    ZEND_ARG_TYPE_INFO(0, data, IS_STRING, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_op_fs);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_dev, 0, 2, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, op, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, arg, IS_STRING, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_op_dev);
PHP_FUNCTION(tenma_debugger);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_reloaded_by, 0, 0, IS_STRING, 1)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_reloaded_by);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_pubsub, 0, 4, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, op, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, id, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, channel, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, message, IS_STRING, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_pubsub);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_durable_context, 0, 1, IS_VOID, 0)
    ZEND_ARG_TYPE_INFO(0, key, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_durable, 0, 4, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, op, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, key, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, number, IS_DOUBLE, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_durable_context);
PHP_FUNCTION(tenma_op_durable);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_plugin_call, 0, 3, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, plugin, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, function, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, input, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_op_plugin_await, 0, 3, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, plugin, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, function, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, input, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_plugin_adopt, 0, 2, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, plugin, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, rid, IS_LONG, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_plugin_call);
PHP_FUNCTION(tenma_op_plugin_await);
PHP_FUNCTION(tenma_plugin_adopt);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_stop_handlers, 0, 1, IS_LONG, 0)
    ZEND_ARG_TYPE_INFO(0, delta, IS_LONG, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_stop_handlers);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_shutdown, 0, 0, IS_VOID, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(tenma_classic_begin);
PHP_FUNCTION(tenma_take_output);
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_tenma_take_output, 0, 0, IS_STRING, 0)
ZEND_END_ARG_INFO()

static const zend_function_entry tenma_functions[] = {
    ZEND_FE(tenma_listen, arginfo_tenma_listen)
    ZEND_FE(tenma_respond, arginfo_tenma_respond)
    ZEND_FE(tenma_op_sleep, arginfo_tenma_op_sleep)
    ZEND_FE(tenma_op_fetch, arginfo_tenma_op_fetch)
    ZEND_FE(tenma_set_input, arginfo_tenma_set_input)
    ZEND_FE(tenma_parse_multipart, arginfo_tenma_parse_multipart)
    ZEND_FE(tenma_forget_session, arginfo_tenma_forget_session)
    ZEND_FE(tenma_stream_start, arginfo_tenma_stream_start)
    ZEND_FE(tenma_stream_write, arginfo_tenma_rid_string_long)
    ZEND_FE(tenma_stream_end, arginfo_tenma_rid_finished)
    ZEND_FE(tenma_ws_accept, arginfo_tenma_rid_bool)
    ZEND_FE(tenma_ws_send, arginfo_tenma_rid_string_long)
    ZEND_FE(tenma_ws_receive, arginfo_tenma_rid_long)
    ZEND_FE(tenma_ws_close, arginfo_tenma_rid_finished)
    ZEND_FE(tenma_op_db, arginfo_tenma_op_db)
    ZEND_FE(tenma_db_abandon, arginfo_tenma_rid_void)
    ZEND_FE(tenma_parse_cookies, arginfo_tenma_parse_cookies)
    ZEND_FE(tenma_op_cancel, arginfo_tenma_op_cancel)
    ZEND_FE(tenma_exit_process, arginfo_tenma_shutdown)
    ZEND_FE(tenma_op_fs, arginfo_tenma_op_fs)
    ZEND_FE(tenma_op_dev, arginfo_tenma_op_dev)
    ZEND_FE(tenma_pubsub, arginfo_tenma_pubsub)
    ZEND_FE(tenma_durable_context, arginfo_tenma_durable_context)
    ZEND_FE(tenma_op_durable, arginfo_tenma_op_durable)
    ZEND_FE(tenma_plugin_call, arginfo_tenma_plugin_call)
    ZEND_FE(tenma_op_plugin_await, arginfo_tenma_op_plugin_await)
    ZEND_FE(tenma_plugin_adopt, arginfo_tenma_plugin_adopt)
    ZEND_FE(tenma_stop_handlers, arginfo_tenma_stop_handlers)
    ZEND_FE(tenma_debugger, arginfo_tenma_shutdown)
    ZEND_FE(tenma_reloaded_by, arginfo_tenma_reloaded_by)
    ZEND_FE(tenma_classic_begin, arginfo_tenma_shutdown)
    ZEND_FE(tenma_take_output, arginfo_tenma_take_output)
    ZEND_FE_END
};

static PHP_MINIT_FUNCTION(tenma);

static zend_module_entry tenma_module = {
    STANDARD_MODULE_HEADER,
    "tenma",
    tenma_functions,
    PHP_MINIT(tenma), NULL, NULL, NULL, NULL,
    /* From Cargo.toml (build.rs): phpversion('tenma') and phpinfo() show the binary's version */
    TENMA_VERSION,
    STANDARD_MODULE_PROPERTIES
};

/** Settings from the command line (-d), as with the PHP CLI; they win over any php.ini. */
static struct php_ini_builder ini_builder;
static int ini_builder_used = 0;

void tenma_define_ini(const char *entry) {
    if (!ini_builder_used) {
        php_ini_builder_init(&ini_builder);
        ini_builder_used = 1;
    }
    php_ini_builder_define(&ini_builder, entry);
}

/** $_SERVER['SERVER_NAME'] in classic mode, never from the Host header. */
static char *server_name = "localhost";

void tenma_set_server_name(const char *name) {
    server_name = strdup(name);
}

/** Like php -c: this php.ini (or this directory) instead of searching for one. */
void tenma_set_ini_path(const char *path) {
    php_embed_module.php_ini_path_override = strdup(path);
}

/**
 * exit() and die() record that they ran before PHP unwinds. Otherwise a script that ends at
 * startup with die("Usage: …") could not be told apart from one that ran to completion, and
 * tenma would still start the timers it had already scheduled.
 */
static zif_handler original_exit = NULL;

static ZEND_NAMED_FUNCTION(tenma_exit) {
    script_exited = 1;
    // exit("text") leaves the status as is; without a reset, an earlier exit(5) in the worker would count
    EG(exit_status) = 0;
    original_exit(INTERNAL_FUNCTION_PARAM_PASSTHRU);
}

static void wrap_exit(void) {
    const char *names[] = {"exit", "die"};
    for (int i = 0; i < 2; i++) {
        zend_function *function = zend_hash_str_find_ptr(CG(function_table), names[i], strlen(names[i]));
        if (function && function->type == ZEND_INTERNAL_FUNCTION) {
            original_exit = function->internal_function.handler;
            function->internal_function.handler = tenma_exit;
        }
    }
}

/** The status PHP would exit with: from exit(), 255 after a fatal error. */
int tenma_exit_status(void) {
    return EG(exit_status);
}

/** 1 if the script last run with tenma_execute ended with exit() or die(). */
int tenma_script_exited(void) {
    return script_exited;
}

void tenma_set_inspect(void) {
    inspect_mode = 1;
}

/** The frame of the statement the debugger stopped at, NULL while PHP runs. */
static _Thread_local zend_execute_data *paused = NULL;
/** Stopped as the paused frame returns: the line of its closing brace and its return value. */
static _Thread_local uint32_t paused_line = 0;
static _Thread_local zval *paused_return = NULL;

static void inspect_throw(zend_object *exception);
/** The debugger runs on this thread: tenma_inspect_thread ran (not on the dev thread). */
static _Thread_local int inspecting = 0;
/** The debugger evaluates code: what it calls must not stop the debugger again. */
static _Thread_local int evaluating = 0;
/**
 * Evaluated code failed fatally while stopped: PHP is unusable. Once the editor lets the code run on,
 * the bailout continues from where it stopped, into the zend_try around the call into PHP.
 */
static _Thread_local int evaluation_fatal = 0;

static void bail_after_fatal_evaluation(void) {
    if (evaluation_fatal) {
        evaluation_fatal = 0;
        zend_bailout();
    }
}
static void (*previous_throw_hook)(zend_object *exception) = NULL;

/**
 * Runs before every statement of code compiled with ZEND_COMPILE_EXTENDED_STMT (see
 * tenma_inspect_thread): Rust decides whether to stop there and blocks the thread while stopped.
 */
static void inspect_statement(zend_execute_data *frame) {
    if (evaluating || !frame->func || !ZEND_USER_CODE(frame->func->type)) {
        return;
    }
    zend_string *file = frame->func->op_array.filename;
    zend_execute_data *outer = paused;
    paused = frame;
    tenma_rs_statement(ZSTR_VAL(file), ZSTR_LEN(file), frame->opline->lineno, (uintptr_t) frame, (uintptr_t) EG(active_fiber));
    paused = outer;
    bail_after_fatal_evaluation();
}

/**
 * A function returns (return, the end of its code or an exception): a breakpoint on its closing
 * brace stops here, whichever way it leaves, with the return value to look at.
 */
static void inspect_return(zend_execute_data *frame, zval *value) {
    if (!inspecting || evaluating || !frame->func || !ZEND_USER_CODE(frame->func->type) || (frame->func->common.fn_flags & ZEND_ACC_GENERATOR)) {
        return;
    }
    zend_op_array *op_array = &frame->func->op_array;
    zend_execute_data *outer = paused;
    paused = frame;
    paused_line = op_array->line_end;
    paused_return = value;
    tenma_rs_return(ZSTR_VAL(op_array->filename), ZSTR_LEN(op_array->filename), op_array->line_end, (uintptr_t) frame, (uintptr_t) EG(active_fiber));
    paused = outer;
    paused_line = 0;
    paused_return = NULL;
    bail_after_fatal_evaluation();
}

/** Without this flag PHP moves a returned variable into the return value, leaving NULL behind. */
static void inspect_call(zend_execute_data *frame) {
    ZEND_ADD_CALL_FLAG(frame, ZEND_CALL_OBSERVED);
}

/** Functions and methods, not a file's top level, whose end is the end of the file. */
static zend_observer_fcall_handlers inspect_observe(zend_execute_data *frame) {
    if (!frame->func || !frame->func->common.function_name) {
        return (zend_observer_fcall_handlers) {NULL, NULL};
    }
    return (zend_observer_fcall_handlers) {inspect_call, inspect_return};
}

/**
 * Output buffers per fiber. PHP keeps one stack of ob_start() buffers per interpreter; with many
 * requests in one worker, a request that waits inside ob_start() (a template asking the database)
 * would leave its buffer open for the next request to write into. So a fiber that is switched
 * away from takes its buffers along, and gets them back when it runs again, as under FPM, where
 * every request has the stack to itself. A fiber starts without buffers, also the ones open where
 * it was started.
 */
typedef struct {
    zend_stack handlers;
    php_output_handler *active;
    php_output_handler *running;
} output_state;

/** The set-aside buffers by fiber context: (zend_ulong) context → output_state *. */
static _Thread_local HashTable *output_states = NULL;

static void output_switch(zend_fiber_context *from, zend_fiber_context *to) {
    bool open = zend_stack_count(&OG(handlers)) > 0;
    if (!open && (!output_states || zend_hash_num_elements(output_states) == 0)) {
        return;
    }
    if (!output_states) {
        output_states = pemalloc(sizeof(HashTable), 1);
        zend_hash_init(output_states, 8, NULL, NULL, 1);
    }
    if (open) {
        output_state *state = pemalloc(sizeof(output_state), 1);
        *state = (output_state) {OG(handlers), OG(active), OG(running)};
        zend_hash_index_update_ptr(output_states, (zend_ulong) (uintptr_t) from, state);
        zend_stack_init(&OG(handlers), sizeof(php_output_handler *));
        OG(active) = NULL;
        OG(running) = NULL;
    }
    output_state *state = zend_hash_index_find_ptr(output_states, (zend_ulong) (uintptr_t) to);
    if (state) {
        zend_stack_destroy(&OG(handlers));
        OG(handlers) = state->handlers;
        OG(active) = state->active;
        OG(running) = state->running;
        zend_hash_index_del(output_states, (zend_ulong) (uintptr_t) to);
        pefree(state, 1);
    }
}

/** A fiber that ended with buffers still open: they go with it, unsent, as at the end of a request. */
static void output_destroy(zend_fiber_context *context) {
    output_state *state = output_states ? zend_hash_index_find_ptr(output_states, (zend_ulong) (uintptr_t) context) : NULL;
    if (!state) {
        return;
    }
    php_output_handler **handler;
    while ((handler = zend_stack_top(&state->handlers))) {
        zend_stack_del_top(&state->handlers);
        php_output_handler_free(handler);
    }
    zend_stack_destroy(&state->handlers);
    zend_hash_index_del(output_states, (zend_ulong) (uintptr_t) context);
    pefree(state, 1);
}

/** Observers are registered while PHP starts its modules, before it knows which are in use. */
static PHP_MINIT_FUNCTION(tenma) {
    if (inspect_mode) {
        zend_observer_fcall_register(inspect_observe);
    }
    zend_observer_fiber_switch_register(output_switch);
    zend_observer_fiber_destroy_register(output_destroy);
    return SUCCESS;
}

/** The debugger moves a breakpoint to the next line where a statement of the same function starts. */
static void inspect_compiled(zend_op_array *op_array) {
    if (!inspecting || !op_array->filename) {
        return;
    }
    uint32_t *lines = malloc((op_array->last + 1) * sizeof(*lines));
    size_t count = 0;
    for (uint32_t i = 0; i < op_array->last; i++) {
        if (op_array->opcodes[i].opcode == ZEND_EXT_STMT) {
            lines[count++] = op_array->opcodes[i].lineno;
        }
    }
    tenma_rs_compiled(ZSTR_VAL(op_array->filename), ZSTR_LEN(op_array->filename), op_array->line_start, op_array->line_end,
        op_array->function_name == NULL, lines, count);
    free(lines);
}

static zend_extension inspector = {
    .name = "tenma inspector",
    .version = PHP_VERSION,
    .op_array_handler = inspect_compiled,
    .statement_handler = inspect_statement,
};

static int tenma_startup(sapi_module_struct *sapi) {
    // php_embed_init has just set its fixed settings; the -d entries go after them
    if (ini_builder_used) {
        php_ini_builder_prepend(&ini_builder, sapi->ini_entries, strlen(sapi->ini_entries));
        sapi->ini_entries = php_ini_builder_finish(&ini_builder);
    }
    if (php_module_startup(sapi, &tenma_module) == FAILURE) {
        return FAILURE;
    }
    if (inspect_mode) {
        // Built in, not loaded from a .so: PHP calls its statement_handler all the same
        zend_register_extension(&inspector, NULL);
        previous_throw_hook = zend_throw_exception_hook;
        zend_throw_exception_hook = inspect_throw;
    }
    return SUCCESS;
}

/** Starts PHP with the tenma module. Afterwards exactly one PHP request runs, until tenma_stop. */
/**
 * Signals belong to Rust: on every request startup PHP installs its own handlers for SIGTERM, SIGINT
 * and co., which pass on to the default action and would kill tenma at once. Remove them before
 * Rust installs its own handlers.
 */
static void release_signals(void) {
    SIGG(reset) = 0;
    const int signals[] = {SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2};
    for (size_t i = 0; i < sizeof(signals) / sizeof(*signals); i++) {
        signal(signals[i], SIG_DFL);
    }
}

/** The body PHP's multipart parser reads via read_post; only set during tenma_parse_multipart (and reset by tenma_thread_start). */
static _Thread_local const char *post_data = NULL;
static _Thread_local size_t post_len = 0, post_pos = 0;

static size_t tenma_read_post(char *buffer, size_t count) {
    size_t left = post_data ? post_len - post_pos : 0;
    size_t n = count < left ? count : left;
    memcpy(buffer, post_data + post_pos, n);
    post_pos += n;
    return n;
}

/** tenma --watch, set before tenma_start. */
static int watch_mode = 0;

void tenma_set_watch(void) {
    watch_mode = 1;
}

#define INI_DEFAULT(name, value) \
    ZVAL_NEW_STR(&tmp, zend_string_init(value, sizeof(value) - 1, 1)); \
    zend_hash_str_update(configuration_hash, name, sizeof(name) - 1, &tmp);

/**
 * Defaults that a php.ini or -d can override. Tenma limits the size of bodies and uploads itself
 * (--max-body). Errors go to the log, which is stderr for tenma, never into a response:
 * in PHP, display_errors=stderr only applies to the cli, cgi and phpdbg SAPIs.
 * Upload progress and fatal error backtraces keep state across requests of one worker, the
 * attack paths of CVE-2026-109619; tenma's worker lives as long as the process, so both are off.
 */
static void tenma_ini_defaults(HashTable *configuration_hash) {
    zval tmp;
    INI_DEFAULT("post_max_size", "0");
    INI_DEFAULT("upload_max_filesize", "0");
    INI_DEFAULT("display_errors", "0");
    INI_DEFAULT("log_errors", "1");
    INI_DEFAULT("session.upload_progress.enabled", "0");
    INI_DEFAULT("fatal_error_backtraces", "0");
    if (watch_mode) {
        // Opcache checks timestamps only every 2 s by default and would load the old code right after saving
        INI_DEFAULT("opcache.revalidate_freq", "0");
        // Not a PHP setting: get_cfg_var('tenma.watch') tells the stdlib (Problems::report) that it runs under --watch
        INI_DEFAULT("tenma.watch", "1");
    }
    if (inspect_mode) {
        // The JIT runs code without the statement opcodes the debugger stops at
        INI_DEFAULT("opcache.enable", "0");
    }
}

/* ---------------------------------------------------------------------------------------------------
 * Asynchronous socket streams. PHP's own transports (tcp, unix, ssl, tls) stay in charge of their
 * descriptors and of OpenSSL; tenma only keeps every descriptor non-blocking and, wherever PHP would
 * poll(), waits for it in an op (src/net.rs) so that only the fiber pauses. mysqlnd (PDO MySQL,
 * mysqli), fsockopen(), stream_socket_client() and the http:// wrapper all go through here. Outside a
 * fiber (script startup, classic mode) the wait is a poll(), as in PHP.
 * ------------------------------------------------------------------------------------------------- */

extern uint64_t tenma_rs_wait_fd(int fd, int events, int64_t timeout_ms);
extern uint64_t tenma_rs_select(const int *fds, const int *events, size_t count, int64_t timeout_ms);
extern int tenma_rs_looping(void);

/* OpenSSL's SSL_want(): the direction a handshake or a record waits for; no header needed for one call */
extern int SSL_want(const void *ssl);
#define TENMA_SSL_WRITING 2
#define TENMA_SSL_READING 3

/* The start of ext/openssl's php_openssl_netstream_data_t (xp_ssl.c, PHP 8.5): what the TLS handshake needs */
struct ssl_stream {
    php_netstream_data_t s;
    void *ssl_handle;
    void *ctx;
    struct timeval connect_timeout;
    int enable_on_connect;
    int is_client;
    int ssl_active;
    int method;
};

static int is_ssl_stream(const php_stream *stream) {
    return stream->ops->label != NULL && strcmp(stream->ops->label, "tcp_socket/ssl") == 0;
}

/* The transport factories PHP registered, replaced by async_factory */
static struct {
    const char *name;
    php_stream_transport_factory original;
} transports[] = {
    {"tcp", NULL}, {"unix", NULL}, {"ssl", NULL}, {"sslv3", NULL}, {"tls", NULL},
    {"tlsv1.0", NULL}, {"tlsv1.1", NULL}, {"tlsv1.2", NULL}, {"tlsv1.3", NULL},
};

/* The ops of the streams those factories make, each with a copy whose read, write and set_option wait in ops */
#define WRAPPED_MAX 6
static struct {
    const php_stream_ops *original;
    php_stream_ops wrapped;
} wrapped_ops[WRAPPED_MAX];
static int wrapped_count = 0;
static pthread_mutex_t wrapped_lock = PTHREAD_MUTEX_INITIALIZER;

static ssize_t async_read(php_stream *stream, char *buf, size_t count);
static ssize_t async_write(php_stream *stream, const char *buf, size_t count);
static int async_set_option(php_stream *stream, int option, int value, void *ptrparam);

static const php_stream_ops *original_ops(const php_stream *stream) {
    for (int i = 0; i < wrapped_count; i++) {
        if (stream->ops == &wrapped_ops[i].wrapped) {
            return wrapped_ops[i].original;
        }
    }
    return stream->ops;
}

static void wrap_ops(php_stream *stream) {
    pthread_mutex_lock(&wrapped_lock);
    for (int i = 0; i < wrapped_count; i++) {
        if (wrapped_ops[i].original == stream->ops || &wrapped_ops[i].wrapped == stream->ops) {
            stream->ops = &wrapped_ops[i].wrapped;
            pthread_mutex_unlock(&wrapped_lock);
            return;
        }
    }
    if (wrapped_count < WRAPPED_MAX) {
        wrapped_ops[wrapped_count].original = stream->ops;
        wrapped_ops[wrapped_count].wrapped = *stream->ops;
        wrapped_ops[wrapped_count].wrapped.read = async_read;
        wrapped_ops[wrapped_count].wrapped.write = async_write;
        wrapped_ops[wrapped_count].wrapped.set_option = async_set_option;
        stream->ops = &wrapped_ops[wrapped_count].wrapped;
        wrapped_count++;
    }
    pthread_mutex_unlock(&wrapped_lock);
}

/** Calls Tenma\Internal\await($op) from C: 1 with the op's value in result, 0 if it threw (the exception is pending). */
static int await_op(uint64_t op, zval *result) {
    zval function, argument;
    ZVAL_STRING(&function, "Tenma\\Internal\\await");
    ZVAL_LONG(&argument, (zend_long) op);
    ZVAL_UNDEF(result);
    call_user_function(NULL, NULL, &function, result, 1, &argument);
    zval_ptr_dtor(&function);
    if (EG(exception)) {
        zval_ptr_dtor(result);
        return 0;
    }
    return 1;
}

static int64_t timeout_ms(const struct timeval *timeout) {
    if (timeout == NULL || timeout->tv_sec < 0) {
        return -1;
    }
    return (int64_t) timeout->tv_sec * 1000 + timeout->tv_usec / 1000;
}

/**
 * Waits until fd is ready for events (POLLIN, POLLOUT): in a fiber as an op, so that other requests
 * run meanwhile; otherwise with poll(). 1: ready. 0: timeout (NULL or tv_sec -1: none). -1: error,
 * or a pending exception (the fiber was cancelled while waiting).
 */
static int wait_fd(int fd, int events, const struct timeval *timeout) {
    if (EG(active_fiber) != NULL && !EG(exception) && tenma_rs_looping()) {
        uint64_t op = tenma_rs_wait_fd(fd, events, timeout_ms(timeout));
        if (op != 0) {
            zval result;
            if (!await_op(op, &result)) {
                return -1;
            }
            int ready = Z_TYPE(result) == IS_STRING && zend_string_equals_literal(Z_STR(result), "ready");
            zval_ptr_dtor(&result);
            return ready;
        }
    }
    struct timeval copy;
    if (timeout != NULL && timeout->tv_sec >= 0) {
        copy = *timeout;
    }
    for (;;) {
        int n = php_pollfd_for(fd, events, timeout != NULL && timeout->tv_sec >= 0 ? &copy : NULL);
        if (n > 0) {
            return 1;
        }
        if (n == 0) {
            return 0;
        }
        if (errno != EINTR) {
            return -1;
        }
    }
}

static int would_block(ssize_t n) {
    return n <= 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR);
}

/** What the socket must become for the operation to go on: for TLS the direction OpenSSL asks for. */
static int direction(const php_stream *stream, int fallback) {
    if (is_ssl_stream(stream)) {
        const struct ssl_stream *ssl = stream->abstract;
        if (ssl->ssl_active && ssl->ssl_handle != NULL) {
            int want = SSL_want(ssl->ssl_handle);
            if (want == TENMA_SSL_READING) {
                return POLLIN | POLLPRI;
            }
            if (want == TENMA_SSL_WRITING) {
                return POLLOUT;
            }
        }
    }
    return fallback;
}

static const struct timeval *read_timeout(const php_netstream_data_t *sock) {
    return sock->timeout.tv_sec == -1 ? NULL : &sock->timeout;
}

static ssize_t async_read(php_stream *stream, char *buf, size_t count) {
    const php_stream_ops *ops = original_ops(stream);
    php_netstream_data_t *sock = stream->abstract;
    // A stream the script set to non-blocking returns at once, as in PHP
    if (sock == NULL || sock->socket == -1 || !sock->is_blocked) {
        return ops->read(stream, buf, count);
    }
    for (;;) {
        // The original ops poll() when they believe the stream blocks: told otherwise, they return at once
        sock->is_blocked = 0;
        errno = 0;
        ssize_t n = ops->read(stream, buf, count);
        sock->is_blocked = 1;
        if (n > 0 || stream->eof || !would_block(n)) {
            return n;
        }
        // PHP does not wait for more while the stream still holds data from an earlier read
        if (stream->has_buffered_data) {
            return 0;
        }
        int ready = wait_fd(sock->socket, direction(stream, POLLIN | POLLPRI), read_timeout(sock));
        if (ready == 0) {
            sock->timeout_event = 1;
            return -1;
        }
        if (ready < 0) {
            return -1;
        }
    }
}

static ssize_t async_write(php_stream *stream, const char *buf, size_t count) {
    const php_stream_ops *ops = original_ops(stream);
    php_netstream_data_t *sock = stream->abstract;
    if (sock == NULL || sock->socket == -1 || !sock->is_blocked) {
        return ops->write(stream, buf, count);
    }
    for (;;) {
        sock->is_blocked = 0;
        errno = 0;
        ssize_t n = ops->write(stream, buf, count);
        sock->is_blocked = 1;
        if (n > 0 || count == 0 || !would_block(n)) {
            return n;
        }
        int ready = wait_fd(sock->socket, direction(stream, POLLOUT), read_timeout(sock));
        if (ready == 0) {
            sock->timeout_event = 1;
            return -1;
        }
        if (ready < 0) {
            return -1;
        }
    }
}

/** The connect has begun on a non-blocking socket: waits for it and reads its outcome. */
static void finish_connect(php_stream_xport_param *xparam, php_netstream_data_t *sock) {
    int ready = wait_fd(sock->socket, POLLOUT, xparam->inputs.timeout);
    if (ready <= 0) {
        xparam->outputs.returncode = -1;
        xparam->outputs.error_code = ready == 0 ? ETIMEDOUT : errno;
        if (xparam->want_errortext) {
            xparam->outputs.error_text = zend_string_init(ready == 0 ? "Connection timed out" : "Connection failed", ready == 0 ? 20 : 17, 0);
        }
        return;
    }
    int error = 0;
    socklen_t len = sizeof(error);
    if (getsockopt(sock->socket, SOL_SOCKET, SO_ERROR, &error, &len) != 0) {
        error = errno;
    }
    xparam->outputs.returncode = error == 0 ? 0 : -1;
    xparam->outputs.error_code = error;
    if (error != 0 && xparam->want_errortext) {
        char *text = php_socket_strerror(error, NULL, 0);
        xparam->outputs.error_text = zend_string_init(text, strlen(text), 0);
        efree(text);
    }
}

static int async_set_option(php_stream *stream, int option, int value, void *ptrparam) {
    const php_stream_ops *ops = original_ops(stream);
    php_netstream_data_t *sock = stream->abstract;
    if (sock == NULL) {
        return ops->set_option(stream, option, value, ptrparam);
    }
    if (option == PHP_STREAM_OPTION_BLOCKING) {
        // The descriptor stays non-blocking; the flag alone decides whether reads and writes wait
        int before = sock->is_blocked;
        sock->is_blocked = value != 0;
        return before;
    }
    if (option == PHP_STREAM_OPTION_XPORT_API) {
        php_stream_xport_param *xparam = ptrparam;
        if (xparam->op != STREAM_XPORT_OP_CONNECT && xparam->op != STREAM_XPORT_OP_CONNECT_ASYNC) {
            return ops->set_option(stream, option, value, ptrparam);
        }
        int wanted = xparam->op;
        // ext/openssl would start the handshake on the connecting socket, which fails on macOS: after the
        // connect instead, below
        struct ssl_stream *ssl = is_ssl_stream(stream) ? stream->abstract : NULL;
        int enable = ssl != NULL ? ssl->enable_on_connect : 0;
        if (ssl != NULL) {
            ssl->enable_on_connect = 0;
        }
        xparam->op = STREAM_XPORT_OP_CONNECT_ASYNC;
        int result = ops->set_option(stream, option, value, ptrparam);
        xparam->op = wanted;
        if (xparam->outputs.returncode == 1 && wanted == STREAM_XPORT_OP_CONNECT) {
            finish_connect(xparam, sock);
            // ponytail: a name with several addresses (localhost: ::1 and 127.0.0.1) gets only its first one
            // tried without blocking; if that fails, PHP's own connect tries the rest, blocking for the time
            // they take. Resolving and trying every address here would remove that
            if (xparam->outputs.returncode == -1 && !EG(exception) && sock->socket != -1) {
                close(sock->socket);
                sock->socket = -1;
                if (xparam->outputs.error_text != NULL) {
                    zend_string_release(xparam->outputs.error_text);
                    xparam->outputs.error_text = NULL;
                }
                result = ops->set_option(stream, option, value, ptrparam);
            }
        }
        if (ssl != NULL) {
            ssl->enable_on_connect = enable;
        }
        if (sock->socket != -1) {
            php_set_sock_blocking(sock->socket, 0);
        }
        if (xparam->outputs.returncode == 0 && ssl != NULL && enable && wanted == STREAM_XPORT_OP_CONNECT) {
            if (php_stream_xport_crypto_setup(stream, ssl->method, NULL) < 0 || php_stream_xport_crypto_enable(stream, 1) < 0) {
                php_error_docref(NULL, E_WARNING, "Failed to enable crypto");
                xparam->outputs.returncode = -1;
            }
        }
        return result;
    }
    if (option == PHP_STREAM_OPTION_CRYPTO_API) {
        php_stream_xport_crypto_param *cparam = ptrparam;
        if (cparam->op != STREAM_XPORT_CRYPTO_OP_ENABLE || !cparam->inputs.activate || !sock->is_blocked) {
            return ops->set_option(stream, option, value, ptrparam);
        }
        struct ssl_stream *ssl = is_ssl_stream(stream) ? stream->abstract : NULL;
        const struct timeval *timeout = ssl != NULL && ssl->is_client ? &ssl->connect_timeout : read_timeout(sock);
        for (;;) {
            sock->is_blocked = 0;
            int result = ops->set_option(stream, option, value, ptrparam);
            sock->is_blocked = 1;
            // 1: done, -1: failed, 0: the handshake waits for the peer
            if (cparam->outputs.returncode != 0) {
                return result;
            }
            int ready = wait_fd(sock->socket, direction(stream, POLLIN | POLLPRI), timeout);
            if (ready <= 0) {
                if (ready == 0) {
                    php_error_docref(NULL, E_WARNING, "SSL: Handshake timed out");
                }
                cparam->outputs.returncode = -1;
                return result;
            }
        }
    }
    return ops->set_option(stream, option, value, ptrparam);
}

/** Makes the stream of PHP's own factory and gives it the waiting ops; servers (bind, listen) stay as they are. */
static php_stream *async_factory(const char *proto, size_t protolen, const char *resourcename, size_t resourcenamelen,
    const char *persistent_id, int options, int flags, struct timeval *timeout, php_stream_context *context STREAMS_DC) {
    php_stream_transport_factory original = NULL;
    for (size_t i = 0; i < sizeof(transports) / sizeof(transports[0]); i++) {
        if (strlen(transports[i].name) == protolen && strncasecmp(transports[i].name, proto, protolen) == 0) {
            original = transports[i].original;
        }
    }
    if (original == NULL) {
        return NULL;
    }
    php_stream *stream = original(proto, protolen, resourcename, resourcenamelen, persistent_id, options, flags, timeout, context STREAMS_REL_CC);
    if (stream != NULL && (flags & STREAM_XPORT_SERVER) == 0 && (flags & (STREAM_XPORT_BIND | STREAM_XPORT_LISTEN)) == 0) {
        wrap_ops(stream);
        php_netstream_data_t *sock = stream->abstract;
        if (sock != NULL && sock->socket != -1) {
            php_set_sock_blocking(sock->socket, 0);
        }
    }
    return stream;
}

static void register_async_transports(void) {
    HashTable *hash = php_stream_xport_get_hash();
    for (size_t i = 0; i < sizeof(transports) / sizeof(transports[0]); i++) {
        php_stream_transport_factory original = zend_hash_str_find_ptr(hash, transports[i].name, strlen(transports[i].name));
        if (original != NULL) {
            transports[i].original = original;
            php_stream_xport_register(transports[i].name, async_factory);
        }
    }
}

/* ---- stream_select() in a fiber: one op waits for the first ready descriptor ---- */

static zif_handler original_stream_select = NULL;

/** The descriptors of one array for select(), or -1 if a stream has none. */
static int collect_fds(zval *array, int events, int *fds, int *fd_events, int *count, int capacity) {
    zval *element;
    ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(array), element) {
        ZVAL_DEREF(element);
        php_stream *stream;
        php_stream_from_zval_no_verify(stream, element);
        php_socket_t fd;
        if (stream == NULL || *count == capacity
            || php_stream_cast(stream, PHP_STREAM_AS_FD_FOR_SELECT | PHP_STREAM_CAST_INTERNAL, (void **) &fd, 1) == FAILURE) {
            return -1;
        }
        fds[*count] = (int) fd;
        fd_events[*count] = events;
        (*count)++;
    } ZEND_HASH_FOREACH_END();
    return 0;
}

/** Keeps the streams whose byte in result has bit set; the array keeps its keys, as PHP's select does. */
static int keep_ready(zval *array, const char *result, int *position, int bit) {
    HashTable *kept = zend_new_array(zend_hash_num_elements(Z_ARRVAL_P(array)));
    zend_ulong index;
    zend_string *key;
    zval *element;
    int ready = 0;
    ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(array), index, key, element) {
        int mask = result[(*position)++] - '0';
        if (mask & bit) {
            zval *copy = key ? zend_hash_update(kept, key, element) : zend_hash_index_update(kept, index, element);
            zval_add_ref(copy);
            ready++;
        }
    } ZEND_HASH_FOREACH_END();
    zval_ptr_dtor(array);
    ZVAL_ARR(array, kept);
    return ready;
}

/** Streams with data in their read buffer are ready at once, as in PHP's stream_select(). */
static int buffered(zval *array) {
    zval *element;
    ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(array), element) {
        ZVAL_DEREF(element);
        php_stream *stream;
        php_stream_from_zval_no_verify(stream, element);
        if (stream != NULL && stream->writepos - stream->readpos > 0) {
            return 1;
        }
    } ZEND_HASH_FOREACH_END();
    return 0;
}

static ZEND_NAMED_FUNCTION(tenma_stream_select) {
    zval *read, *write, *except;
    zend_long sec, usec = 0;
    bool sec_null, usec_null = 1;
    if (EG(active_fiber) == NULL || !tenma_rs_looping()) {
        original_stream_select(INTERNAL_FUNCTION_PARAM_PASSTHRU);
        return;
    }
    ZEND_PARSE_PARAMETERS_START(4, 5)
        Z_PARAM_ARRAY_EX2(read, 1, 1, 0)
        Z_PARAM_ARRAY_EX2(write, 1, 1, 0)
        Z_PARAM_ARRAY_EX2(except, 1, 1, 0)
        Z_PARAM_LONG_OR_NULL(sec, sec_null)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG_OR_NULL(usec, usec_null)
    ZEND_PARSE_PARAMETERS_END();

    int fds[FD_SETSIZE], events[FD_SETSIZE], count = 0;
    zval *arrays[3] = {read, write, except};
    int bits[3] = {POLLIN | POLLPRI, POLLOUT, POLLPRI};
    int unsupported = 0;
    if (read != NULL && buffered(read)) {
        unsupported = 1;
    }
    // Out-of-band data has no readiness of its own in the op: PHP's own select handles an except set
    if (except != NULL && zend_hash_num_elements(Z_ARRVAL_P(except)) > 0) {
        unsupported = 1;
    }
    for (int i = 0; i < 3 && !unsupported; i++) {
        if (arrays[i] != NULL && collect_fds(arrays[i], bits[i], fds, events, &count, FD_SETSIZE) < 0) {
            unsupported = 1;
        }
    }
    // Buffered data, a stream without a descriptor, bad arguments: PHP's own select handles them
    if (unsupported || count == 0 || (!sec_null && (sec < 0 || usec < 0))) {
        original_stream_select(INTERNAL_FUNCTION_PARAM_PASSTHRU);
        return;
    }
    int64_t ms = sec_null ? -1 : sec * 1000 + usec / 1000;
    uint64_t op = tenma_rs_select(fds, events, (size_t) count, ms);
    if (op == 0) {
        original_stream_select(INTERNAL_FUNCTION_PARAM_PASSTHRU);
        return;
    }
    zval result;
    if (!await_op(op, &result)) {
        RETURN_THROWS();
    }
    if (Z_TYPE(result) != IS_STRING || Z_STRLEN(result) != (size_t) count) {
        zval_ptr_dtor(&result);
        RETURN_FALSE;
    }
    int ready = 0, position = 0;
    int keep[3] = {1, 2, 1};
    for (int i = 0; i < 3; i++) {
        if (arrays[i] != NULL) {
            ready += keep_ready(arrays[i], Z_STRVAL(result), &position, keep[i]);
        }
    }
    zval_ptr_dtor(&result);
    RETURN_LONG(ready);
}

static void wrap_stream_select(void) {
    zend_function *function = zend_hash_str_find_ptr(CG(function_table), "stream_select", strlen("stream_select"));
    if (function != NULL && function->type == ZEND_INTERNAL_FUNCTION) {
        original_stream_select = function->internal_function.handler;
        function->internal_function.handler = tenma_stream_select;
    }
}

/**
 * session_start() only inside a classic request. In fiber mode one worker serves many visitors at
 * once and there is no session cookie: the session would pass from one visitor to the next.
 */
static zif_handler original_session_start = NULL;

static ZEND_NAMED_FUNCTION(tenma_session_start) {
    if (!classic_open) {
        zend_throw_error(
            NULL, "session_start() works only in classic mode: in fiber mode visitors would share one session"
        );
        RETURN_THROWS();
    }
    original_session_start(INTERNAL_FUNCTION_PARAM_PASSTHRU);
}

static void wrap_session_start(void) {
    zend_function *function = zend_hash_str_find_ptr(CG(function_table), "session_start", strlen("session_start"));
    if (function != NULL && function->type == ZEND_INTERNAL_FUNCTION) {
        original_session_start = function->internal_function.handler;
        function->internal_function.handler = tenma_session_start;
    }
}

/* ---------------------------------------------------------------------------------------------------
 * The PDO driver "pgsql" of tenma, on tokio-postgres (src/pdo.rs) instead of libpq: inside a fiber a
 * query suspends only the fiber. It takes the place of ext/pdo_pgsql unless --pdo-pgsql native is given.
 * Values arrive as the text Postgres prints, typed by OID as pdo_pgsql types them. Not covered: large
 * objects, the Pdo\Pgsql class with its pgsql-specific methods, COPY and unbuffered fetching.
 * ------------------------------------------------------------------------------------------------- */

#include <ext/pdo/php_pdo.h>
#include <ext/pdo/php_pdo_driver.h>
#include <main/php_memory_streams.h>

struct pdo_result {
    uint8_t *ptr;
    size_t len;
    int error;
};

extern uint64_t tenma_rs_pdo(const char *op, size_t op_len, uint64_t id, const char *sql, size_t sql_len,
    const char *params, size_t params_len, int blocking, struct pdo_result *out);
extern void tenma_rs_pdo_free(uint8_t *ptr, size_t len);
extern void tenma_rs_pdo_close(uint64_t id);
extern const pdo_driver_t pdo_pgsql_driver;
extern int pdo_pgsql_scanner(pdo_scanner_t *s);
static zend_string *exception_message(zend_object *exception);

static int pdo_native = 0;

/** --pdo-pgsql native: PDO keeps libpq. */
void tenma_set_pdo_native(void) {
    pdo_native = 1;
}

#define PG_BOOLOID 16
#define PG_BYTEAOID 17
#define PG_INT8OID 20
#define PG_INT2OID 21
#define PG_INT4OID 23
#define PG_TEXTOID 25
#define PG_OIDOID 26
#define PG_FLOAT4OID 700
#define PG_FLOAT8OID 701
#define PG_VARCHAROID 1043
#define PG_DATEOID 1082
#define PG_TIMESTAMPOID 1114
#define PG_NULL 0xFFFFFFFFu

typedef struct {
    uint64_t id;
    int emulate_prepares;
    int errcode;
    char *errmsg;
    int persistent;
} tenma_pg_handle;

typedef struct {
    tenma_pg_handle *H;
    zend_string *query;
    /** The result as src/pdo.rs packs it; the arrays below point into it. */
    zend_string *buffer;
    uint32_t columns;
    uint64_t rows;
    uint64_t affected;
    /** Rows fetched so far; the current row is current - 1. */
    uint64_t current;
    const char **names;
    uint32_t *name_lengths;
    uint32_t *oids;
    const uint8_t **row_starts;
    /** Bound parameters by position, strings or null, as the server will see them. */
    zval *params;
    uint32_t param_count;
} tenma_pg_stmt;

static const struct pdo_dbh_methods tenma_pg_methods;
static const struct pdo_stmt_methods tenma_pg_stmt_methods;

/** Records an error "SQLSTATE|message" (as src/pdo.rs reports it) on the handle or the statement. */
static void pg_record_error(pdo_dbh_t *dbh, pdo_stmt_t *stmt, const char *text, size_t len) {
    tenma_pg_handle *H = dbh->driver_data;
    pdo_error_type *code = stmt != NULL ? &stmt->error_code : &dbh->error_code;
    const char *bar = memchr(text, '|', len);
    if (bar != NULL && bar - text == 5) {
        memcpy(*code, text, 5);
        (*code)[5] = '\0';
        text = bar + 1;
        len -= 6;
    } else {
        strcpy(*code, "HY000");
    }
    if (H->errmsg != NULL) {
        pefree(H->errmsg, H->persistent);
    }
    H->errmsg = pemalloc(len + 1, H->persistent);
    memcpy(H->errmsg, text, len);
    H->errmsg[len] = '\0';
    // What pdo_pgsql puts there: libpq's PGRES_FATAL_ERROR
    H->errcode = 7;
    if (dbh->methods == NULL) {
        pdo_throw_exception(H->errcode, H->errmsg, code);
    }
}

/**
 * Runs an op of src/pdo.rs: "connect", "query" or "exec". 1 with the result in *buffer, 0 after an
 * error, recorded on the handle or statement; a cancellation of the fiber stays pending as an exception.
 */
static int pg_call(pdo_dbh_t *dbh, pdo_stmt_t *stmt, const char *op, const char *sql, size_t sql_len,
    const char *params, size_t params_len, zend_string **buffer) {
    tenma_pg_handle *H = dbh->driver_data;
    *buffer = NULL;
    struct pdo_result out = {NULL, 0, 0};
    uint64_t id = tenma_rs_pdo(op, strlen(op), H->id, sql, sql_len, params, params_len, EG(active_fiber) == NULL, &out);
    if (id != 0) {
        zval result;
        if (!await_op(id, &result)) {
            // The op failed: await() threw a RuntimeException with the server's message. Anything
            // else (Cancelled) is not the database's business and stays pending
            if (EG(exception) != NULL && EG(exception)->ce == spl_ce_RuntimeException) {
                zend_string *borrowed = exception_message(EG(exception));
                zend_string *message = borrowed != NULL ? zend_string_copy(borrowed) : ZSTR_EMPTY_ALLOC();
                zend_clear_exception();
                pg_record_error(dbh, stmt, ZSTR_VAL(message), ZSTR_LEN(message));
                zend_string_release(message);
            }
            return 0;
        }
        if (Z_TYPE(result) != IS_STRING) {
            zval_ptr_dtor(&result);
            pg_record_error(dbh, stmt, "HY000|unexpected result", 23);
            return 0;
        }
        *buffer = Z_STR(result);
        return 1;
    }
    if (out.error) {
        pg_record_error(dbh, stmt, (const char *) out.ptr, out.len);
        tenma_rs_pdo_free(out.ptr, out.len);
        return 0;
    }
    *buffer = zend_string_init((const char *) out.ptr, out.len, 0);
    tenma_rs_pdo_free(out.ptr, out.len);
    return 1;
}

static uint32_t read_u32(const uint8_t *p) {
    return (uint32_t) p[0] | (uint32_t) p[1] << 8 | (uint32_t) p[2] << 16 | (uint32_t) p[3] << 24;
}

static uint64_t read_u64(const uint8_t *p) {
    return (uint64_t) read_u32(p) | (uint64_t) read_u32(p + 4) << 32;
}

static void pg_free_result(tenma_pg_stmt *S) {
    if (S->buffer != NULL) {
        zend_string_release(S->buffer);
        S->buffer = NULL;
    }
    efree(S->names);
    efree(S->name_lengths);
    efree(S->oids);
    efree(S->row_starts);
    S->names = NULL;
    S->name_lengths = NULL;
    S->oids = NULL;
    S->row_starts = NULL;
    S->columns = 0;
    S->rows = 0;
    S->affected = 0;
    S->current = 0;
}

/** Takes a result buffer apart: columns, then where every row starts. 0 if it is malformed. */
static int pg_take_result(tenma_pg_stmt *S, zend_string *buffer) {
    pg_free_result(S);
    S->buffer = buffer;
    const uint8_t *p = (const uint8_t *) ZSTR_VAL(buffer);
    const uint8_t *end = p + ZSTR_LEN(buffer);
    if (end - p < 4) {
        return 0;
    }
    S->columns = read_u32(p);
    p += 4;
    S->names = safe_emalloc(S->columns, sizeof(char *), 0);
    S->name_lengths = safe_emalloc(S->columns, sizeof(uint32_t), 0);
    S->oids = safe_emalloc(S->columns, sizeof(uint32_t), 0);
    for (uint32_t i = 0; i < S->columns; i++) {
        if (end - p < 4) {
            return 0;
        }
        uint32_t len = read_u32(p);
        p += 4;
        if ((size_t) (end - p) < (size_t) len + 4) {
            return 0;
        }
        S->names[i] = (const char *) p;
        S->name_lengths[i] = len;
        p += len;
        S->oids[i] = read_u32(p);
        p += 4;
    }
    if (end - p < 16) {
        return 0;
    }
    S->rows = read_u64(p);
    S->affected = read_u64(p + 8);
    p += 16;
    S->row_starts = safe_emalloc((size_t) S->rows, sizeof(uint8_t *), 0);
    for (uint64_t row = 0; row < S->rows; row++) {
        S->row_starts[row] = p;
        for (uint32_t i = 0; i < S->columns; i++) {
            if (end - p < 4) {
                return 0;
            }
            uint32_t len = read_u32(p);
            p += 4;
            if (len != PG_NULL) {
                if ((size_t) (end - p) < len) {
                    return 0;
                }
                p += len;
            }
        }
    }
    return 1;
}

/** The value of a column in the current row; 0 for NULL. */
static int pg_value(tenma_pg_stmt *S, uint32_t column, const char **value, size_t *len) {
    const uint8_t *p = S->row_starts[S->current - 1];
    for (uint32_t i = 0; i <= column; i++) {
        uint32_t size = read_u32(p);
        p += 4;
        if (i == column) {
            if (size == PG_NULL) {
                return 0;
            }
            *value = (const char *) p;
            *len = size;
            return 1;
        }
        if (size != PG_NULL) {
            p += size;
        }
    }
    return 0;
}

static void pg_handle_closer(pdo_dbh_t *dbh) {
    tenma_pg_handle *H = dbh->driver_data;
    if (H == NULL) {
        return;
    }
    if (H->id != 0) {
        tenma_rs_pdo_close(H->id);
    }
    if (H->errmsg != NULL) {
        pefree(H->errmsg, H->persistent);
    }
    pefree(H, H->persistent);
    dbh->driver_data = NULL;
}

static bool pg_handle_preparer(pdo_dbh_t *dbh, zend_string *sql, pdo_stmt_t *stmt, zval *driver_options) {
    tenma_pg_handle *H = dbh->driver_data;
    tenma_pg_stmt *S = ecalloc(1, sizeof(tenma_pg_stmt));
    S->H = H;
    stmt->driver_data = S;
    stmt->methods = &tenma_pg_stmt_methods;
    int emulate = driver_options != NULL
        ? pdo_attr_lval(driver_options, PDO_ATTR_EMULATE_PREPARES, H->emulate_prepares) == 1
        : H->emulate_prepares;
    if (emulate) {
        stmt->supports_placeholders = PDO_PLACEHOLDER_NONE;
    } else {
        stmt->supports_placeholders = PDO_PLACEHOLDER_NAMED;
        stmt->named_rewrite_template = "$%d";
    }
    zend_string *rewritten = NULL;
    int result = pdo_parse_params(stmt, sql, &rewritten);
    if (result == -1) {
        strcpy(dbh->error_code, stmt->error_code);
        return false;
    }
    S->query = result == 1 ? rewritten : zend_string_copy(sql);
    return true;
}

static zend_long pg_handle_doer(pdo_dbh_t *dbh, const zend_string *sql) {
    zend_string *buffer;
    if (!pg_call(dbh, NULL, "exec", ZSTR_VAL(sql), ZSTR_LEN(sql), NULL, 0, &buffer)) {
        return -1;
    }
    tenma_pg_stmt scratch = {0};
    zend_long affected = pg_take_result(&scratch, buffer) ? (zend_long) scratch.affected : -1;
    pg_free_result(&scratch);
    return affected;
}

static zend_string *pg_handle_quoter(pdo_dbh_t *dbh, const zend_string *unquoted, enum pdo_param_type paramtype) {
    smart_str quoted = {0};
    if (paramtype == PDO_PARAM_LOB) {
        // bytea as hex: '\x6869'
        static const char hex[] = "0123456789abcdef";
        smart_str_appendl(&quoted, "'\\x", 3);
        for (size_t i = 0; i < ZSTR_LEN(unquoted); i++) {
            unsigned char byte = (unsigned char) ZSTR_VAL(unquoted)[i];
            smart_str_appendc(&quoted, hex[byte >> 4]);
            smart_str_appendc(&quoted, hex[byte & 15]);
        }
        smart_str_appendc(&quoted, '\'');
        smart_str_0(&quoted);
        return quoted.s;
    }
    // ponytail: standard_conforming_strings is on since Postgres 9.1, so only the quote needs doubling;
    // a server with it off would read backslashes as escapes
    if (memchr(ZSTR_VAL(unquoted), '\0', ZSTR_LEN(unquoted)) != NULL) {
        return NULL;
    }
    smart_str_appendc(&quoted, '\'');
    for (size_t i = 0; i < ZSTR_LEN(unquoted); i++) {
        char c = ZSTR_VAL(unquoted)[i];
        if (c == '\'') {
            smart_str_appendc(&quoted, '\'');
        }
        smart_str_appendc(&quoted, c);
    }
    smart_str_appendc(&quoted, '\'');
    smart_str_0(&quoted);
    return quoted.s;
}

static bool pg_handle_command(pdo_dbh_t *dbh, const char *command) {
    zend_string *buffer;
    if (!pg_call(dbh, NULL, "exec", command, strlen(command), NULL, 0, &buffer)) {
        return false;
    }
    zend_string_release(buffer);
    return true;
}

static bool pg_handle_begin(pdo_dbh_t *dbh) {
    return pg_handle_command(dbh, "BEGIN");
}

static bool pg_handle_commit(pdo_dbh_t *dbh) {
    return pg_handle_command(dbh, "COMMIT");
}

static bool pg_handle_rollback(pdo_dbh_t *dbh) {
    return pg_handle_command(dbh, "ROLLBACK");
}

static bool pg_handle_set_attribute(pdo_dbh_t *dbh, zend_long attr, zval *value) {
    tenma_pg_handle *H = dbh->driver_data;
    switch (attr) {
        case PDO_ATTR_EMULATE_PREPARES:
            {
                bool flag;
                if (!pdo_get_bool_param(&flag, value)) {
                    return false;
                }
                H->emulate_prepares = flag;
                return true;
            }
        // Accepted for scripts written for pdo_pgsql; every statement is prepared and fully fetched here
        case PDO_ATTR_DRIVER_SPECIFIC:
        case PDO_ATTR_PREFETCH:
            return true;
        default:
            return false;
    }
}

/** The first value of the first row of a one-column result, or NULL. */
static zend_string *pg_single_value(pdo_dbh_t *dbh, const char *sql, const char *params, size_t params_len) {
    zend_string *buffer;
    if (!pg_call(dbh, NULL, "query", sql, strlen(sql), params, params_len, &buffer)) {
        return NULL;
    }
    tenma_pg_stmt scratch = {0};
    zend_string *value = NULL;
    if (pg_take_result(&scratch, buffer) && scratch.rows > 0 && scratch.columns > 0) {
        scratch.current = 1;
        const char *text;
        size_t len;
        if (pg_value(&scratch, 0, &text, &len)) {
            value = zend_string_init(text, len, 0);
        }
    }
    pg_free_result(&scratch);
    return value;
}

/** Parameters for src/pdo.rs: each as 4 bytes of length and its bytes, length 0xFFFFFFFF for NULL. */
static void pg_pack_param(smart_str *out, const char *value, size_t len) {
    uint32_t size = value == NULL ? PG_NULL : (uint32_t) len;
    char head[4] = {(char) size, (char) (size >> 8), (char) (size >> 16), (char) (size >> 24)};
    smart_str_appendl(out, head, 4);
    if (value != NULL) {
        smart_str_appendl(out, value, len);
    }
}

static zend_string *pg_handle_last_id(pdo_dbh_t *dbh, const zend_string *name) {
    if (name == NULL) {
        return pg_single_value(dbh, "SELECT LASTVAL()", NULL, 0);
    }
    smart_str params = {0};
    pg_pack_param(&params, ZSTR_VAL(name), ZSTR_LEN(name));
    zend_string *id = pg_single_value(dbh, "SELECT CURRVAL($1)", ZSTR_VAL(params.s), ZSTR_LEN(params.s));
    smart_str_free(&params);
    return id;
}

static void pg_handle_fetch_error(pdo_dbh_t *dbh, pdo_stmt_t *stmt, zval *info) {
    tenma_pg_handle *H = dbh->driver_data;
    if (H->errcode) {
        add_next_index_long(info, H->errcode);
    } else {
        add_next_index_null(info);
    }
    if (H->errmsg != NULL) {
        add_next_index_string(info, H->errmsg);
    }
}

static int pg_handle_get_attribute(pdo_dbh_t *dbh, zend_long attr, zval *value) {
    tenma_pg_handle *H = dbh->driver_data;
    switch (attr) {
        case PDO_ATTR_EMULATE_PREPARES:
            ZVAL_BOOL(value, H->emulate_prepares);
            return 1;
        case PDO_ATTR_DRIVER_SPECIFIC:
            ZVAL_FALSE(value);
            return 1;
        case PDO_ATTR_CLIENT_VERSION:
            ZVAL_STRING(value, "tenma");
            return 1;
        case PDO_ATTR_SERVER_VERSION: {
            zend_string *version = pg_single_value(dbh, "SHOW server_version", NULL, 0);
            if (version == NULL) {
                return -1;
            }
            ZVAL_STR(value, version);
            return 1;
        }
        case PDO_ATTR_CONNECTION_STATUS:
            ZVAL_STRING(value, "Connection OK; waiting to send.");
            return 1;
        default:
            return 0;
    }
}

static const struct pdo_dbh_methods tenma_pg_methods = {
    pg_handle_closer,
    pg_handle_preparer,
    pg_handle_doer,
    pg_handle_quoter,
    pg_handle_begin,
    pg_handle_commit,
    pg_handle_rollback,
    pg_handle_set_attribute,
    pg_handle_last_id,
    pg_handle_fetch_error,
    pg_handle_get_attribute,
    NULL, /* check_liveness */
    NULL, /* get_driver_methods */
    NULL, /* persistent_shutdown */
    NULL, /* in_transaction: PDO keeps track */
    NULL, /* get_gc */
    pdo_pgsql_scanner,
};

/* ---- statements ---- */

static void pg_free_params(tenma_pg_stmt *S) {
    if (S->params != NULL) {
        for (uint32_t i = 0; i < S->param_count; i++) {
            zval_ptr_dtor(&S->params[i]);
        }
        efree(S->params);
        S->params = NULL;
        S->param_count = 0;
    }
}

static int pg_stmt_dtor(pdo_stmt_t *stmt) {
    tenma_pg_stmt *S = stmt->driver_data;
    if (S == NULL) {
        return 1;
    }
    pg_free_result(S);
    pg_free_params(S);
    if (S->query != NULL) {
        zend_string_release(S->query);
    }
    efree(S);
    stmt->driver_data = NULL;
    return 1;
}

static int pg_stmt_execute(pdo_stmt_t *stmt) {
    tenma_pg_stmt *S = stmt->driver_data;
    smart_str params = {0};
    // bound_param_map holds every placeholder twice (by name and by position): the bound parameters count
    uint32_t count = stmt->bound_params != NULL ? zend_hash_num_elements(stmt->bound_params) : 0;
    for (uint32_t i = 0; i < count && i < S->param_count; i++) {
        zval *param = &S->params[i];
        if (Z_TYPE_P(param) == IS_STRING) {
            pg_pack_param(&params, Z_STRVAL_P(param), Z_STRLEN_P(param));
        } else {
            pg_pack_param(&params, NULL, 0);
        }
    }
    // With emulated prepares PDO has put the values into the query itself
    zend_string *sql = stmt->supports_placeholders == PDO_PLACEHOLDER_NONE ? stmt->active_query_string : S->query;
    zend_string *buffer;
    int ok = pg_call(stmt->dbh, stmt, "query", ZSTR_VAL(sql), ZSTR_LEN(sql),
        params.s != NULL ? ZSTR_VAL(params.s) : NULL, params.s != NULL ? ZSTR_LEN(params.s) : 0, &buffer);
    smart_str_free(&params);
    if (!ok) {
        return 0;
    }
    if (!pg_take_result(S, buffer)) {
        pg_free_result(S);
        pg_record_error(stmt->dbh, stmt, "HY000|malformed result", 22);
        return 0;
    }
    php_pdo_stmt_set_column_count(stmt, (int) S->columns);
    stmt->row_count = (zend_long) (S->columns == 0 ? S->affected : S->rows);
    return 1;
}

static int pg_stmt_fetch(pdo_stmt_t *stmt, enum pdo_fetch_orientation ori, zend_long offset) {
    tenma_pg_stmt *S = stmt->driver_data;
    if (S->buffer == NULL) {
        return 0;
    }
    // Every row is in memory: scrolling costs nothing, so every orientation works
    int64_t target;
    switch (ori) {
        case PDO_FETCH_ORI_NEXT:
            target = (int64_t) S->current + 1;
            break;
        case PDO_FETCH_ORI_PRIOR:
            target = (int64_t) S->current - 1;
            break;
        case PDO_FETCH_ORI_FIRST:
            target = 1;
            break;
        case PDO_FETCH_ORI_LAST:
            target = (int64_t) S->rows;
            break;
        case PDO_FETCH_ORI_ABS:
            target = offset;
            break;
        case PDO_FETCH_ORI_REL:
            target = (int64_t) S->current + offset;
            break;
        default:
            return 0;
    }
    if (target < 1 || target > (int64_t) S->rows) {
        return 0;
    }
    S->current = (uint64_t) target;
    return 1;
}

static int pg_stmt_describe(pdo_stmt_t *stmt, int colno) {
    tenma_pg_stmt *S = stmt->driver_data;
    if (S->buffer == NULL || colno < 0 || (uint32_t) colno >= S->columns) {
        return 0;
    }
    stmt->columns[colno].name = zend_string_init(S->names[colno], S->name_lengths[colno], 0);
    // As pdo_pgsql reports a variable-length column: PQfsize and PQfmod give -1
    stmt->columns[colno].maxlen = (size_t) -1;
    stmt->columns[colno].precision = (zend_ulong) -1;
    return 1;
}

static int hex_digit(char c) {
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    if (c >= 'A' && c <= 'F') {
        return c - 'A' + 10;
    }
    return -1;
}

/** bytea as Postgres prints it, '\x' and hex, back to bytes; escape format bytes stay as they are. */
static zend_string *pg_unescape_bytea(const char *text, size_t len) {
    if (len < 2 || text[0] != '\\' || text[1] != 'x') {
        return zend_string_init(text, len, 0);
    }
    zend_string *bytes = zend_string_alloc((len - 2) / 2, 0);
    size_t n = 0;
    for (size_t i = 2; i + 1 < len; i += 2) {
        int high = hex_digit(text[i]), low = hex_digit(text[i + 1]);
        if (high < 0 || low < 0) {
            break;
        }
        ZSTR_VAL(bytes)[n++] = (char) (high << 4 | low);
    }
    ZSTR_VAL(bytes)[n] = '\0';
    ZSTR_LEN(bytes) = n;
    return bytes;
}

static int pg_stmt_get_col(pdo_stmt_t *stmt, int colno, zval *result, enum pdo_param_type *type) {
    tenma_pg_stmt *S = stmt->driver_data;
    if (S->buffer == NULL || S->current == 0 || colno < 0 || (uint32_t) colno >= S->columns) {
        return 0;
    }
    const char *value;
    size_t len;
    if (!pg_value(S, (uint32_t) colno, &value, &len)) {
        ZVAL_NULL(result);
        return 1;
    }
    switch (S->oids[colno]) {
        case PG_BOOLOID:
            ZVAL_BOOL(result, len > 0 && value[0] == 't');
            break;
        case PG_INT2OID:
        case PG_INT4OID:
        case PG_INT8OID:
        case PG_OIDOID:
            ZVAL_LONG(result, ZEND_STRTOL(value, NULL, 10));
            break;
        case PG_FLOAT4OID:
        case PG_FLOAT8OID:
            if (len == 8 && memcmp(value, "Infinity", 8) == 0) {
                ZVAL_DOUBLE(result, ZEND_INFINITY);
            } else if (len == 9 && memcmp(value, "-Infinity", 9) == 0) {
                ZVAL_DOUBLE(result, -ZEND_INFINITY);
            } else if (len == 3 && memcmp(value, "NaN", 3) == 0) {
                ZVAL_DOUBLE(result, ZEND_NAN);
            } else {
                ZVAL_DOUBLE(result, zend_strtod(value, NULL));
            }
            break;
        case PG_BYTEAOID: {
            // As pdo_pgsql: a stream over the bytes
            zend_string *bytes = pg_unescape_bytea(value, len);
            php_stream *stream = php_stream_memory_open(TEMP_STREAM_READONLY, bytes);
            php_stream_to_zval(stream, result);
            zend_string_release(bytes);
            break;
        }
        default:
            ZVAL_STRINGL(result, value, len);
    }
    return 1;
}

/** A parameter's value as the server will read it: null, 't'/'f', bytea as hex, otherwise its string. */
static void pg_param_value(zval *target, zval *parameter, enum pdo_param_type type) {
    if (Z_ISREF_P(parameter)) {
        parameter = Z_REFVAL_P(parameter);
    }
    zval_ptr_dtor(target);
    if (type == PDO_PARAM_NULL || Z_TYPE_P(parameter) == IS_NULL) {
        ZVAL_NULL(target);
        return;
    }
    if (Z_TYPE_P(parameter) == IS_TRUE || Z_TYPE_P(parameter) == IS_FALSE) {
        ZVAL_STRINGL(target, Z_TYPE_P(parameter) == IS_TRUE ? "t" : "f", 1);
        return;
    }
    if (type == PDO_PARAM_LOB) {
        zend_string *bytes;
        if (Z_TYPE_P(parameter) == IS_RESOURCE) {
            php_stream *stream;
            php_stream_from_zval_no_verify(stream, parameter);
            bytes = stream != NULL ? php_stream_copy_to_mem(stream, PHP_STREAM_COPY_ALL, 0) : NULL;
            if (bytes == NULL) {
                bytes = ZSTR_EMPTY_ALLOC();
            }
        } else {
            bytes = zval_get_string(parameter);
        }
        zend_string *hex = zend_string_alloc(ZSTR_LEN(bytes) * 2 + 2, 0);
        static const char digits[] = "0123456789abcdef";
        ZSTR_VAL(hex)[0] = '\\';
        ZSTR_VAL(hex)[1] = 'x';
        for (size_t i = 0; i < ZSTR_LEN(bytes); i++) {
            unsigned char byte = (unsigned char) ZSTR_VAL(bytes)[i];
            ZSTR_VAL(hex)[2 + i * 2] = digits[byte >> 4];
            ZSTR_VAL(hex)[3 + i * 2] = digits[byte & 15];
        }
        ZSTR_VAL(hex)[ZSTR_LEN(hex)] = '\0';
        zend_string_release(bytes);
        ZVAL_STR(target, hex);
        return;
    }
    ZVAL_STR(target, zval_get_string(parameter));
}

static int pg_stmt_param_hook(pdo_stmt_t *stmt, struct pdo_bound_param_data *param, enum pdo_param_event event) {
    tenma_pg_stmt *S = stmt->driver_data;
    if (!param->is_param) {
        return 1;
    }
    if (stmt->supports_placeholders != PDO_PLACEHOLDER_NAMED) {
        // Emulated: PDO quotes the value into the query, a bool must become 't' or 'f' first
        if (event == PDO_PARAM_EVT_NORMALIZE && PDO_PARAM_TYPE(param->param_type) == PDO_PARAM_BOOL
            && (param->param_type & PDO_PARAM_INPUT_OUTPUT) != PDO_PARAM_INPUT_OUTPUT) {
            const char *flag = zend_is_true(&param->parameter) ? "t" : "f";
            param->param_type = PDO_PARAM_STR;
            zval_ptr_dtor(&param->parameter);
            ZVAL_STRINGL(&param->parameter, flag, 1);
        }
        return 1;
    }
    switch (event) {
        case PDO_PARAM_EVT_NORMALIZE:
            // $1, $2 or a name PDO rewrote to one: the position
            if (param->name != NULL) {
                if (ZSTR_VAL(param->name)[0] == '$') {
                    param->paramno = ZEND_ATOL(ZSTR_VAL(param->name) + 1) - 1;
                } else {
                    zend_string *rewritten = stmt->bound_param_map != NULL
                        ? zend_hash_find_ptr(stmt->bound_param_map, param->name)
                        : NULL;
                    if (rewritten == NULL) {
                        pg_record_error(stmt->dbh, stmt, "HY093|parameter was not defined", 31);
                        return 0;
                    }
                    param->paramno = ZEND_ATOL(ZSTR_VAL(rewritten) + 1) - 1;
                }
            }
            return 1;
        case PDO_PARAM_EVT_ALLOC:
            if (stmt->bound_param_map != NULL && !zend_hash_index_exists(stmt->bound_param_map, param->paramno)) {
                pg_record_error(stmt->dbh, stmt, "HY093|parameter was not defined", 31);
                return 0;
            }
            return 1;
        case PDO_PARAM_EVT_EXEC_PRE: {
            if (stmt->bound_param_map == NULL) {
                return 1;
            }
            uint32_t count = zend_hash_num_elements(stmt->bound_param_map);
            if (S->params == NULL || S->param_count != count) {
                pg_free_params(S);
                S->params = ecalloc(count, sizeof(zval));
                S->param_count = count;
            }
            if (param->paramno >= 0 && (uint32_t) param->paramno < count) {
                pg_param_value(&S->params[param->paramno], &param->parameter, PDO_PARAM_TYPE(param->param_type));
            }
            return 1;
        }
        default:
            return 1;
    }
}

static int pg_stmt_get_column_meta(pdo_stmt_t *stmt, zend_long colno, zval *return_value) {
    tenma_pg_stmt *S = stmt->driver_data;
    if (S->buffer == NULL || colno < 0 || colno >= S->columns) {
        return FAILURE;
    }
    array_init(return_value);
    uint32_t oid = S->oids[colno];
    add_assoc_long(return_value, "pgsql:oid", oid);
    const char *native = NULL;
    enum pdo_param_type type = PDO_PARAM_STR;
    switch (oid) {
        case PG_BOOLOID: native = "bool"; type = PDO_PARAM_BOOL; break;
        case PG_BYTEAOID: native = "bytea"; type = PDO_PARAM_LOB; break;
        case PG_INT8OID: native = "int8"; type = PDO_PARAM_INT; break;
        case PG_INT2OID: native = "int2"; type = PDO_PARAM_INT; break;
        case PG_INT4OID: native = "int4"; type = PDO_PARAM_INT; break;
        case PG_OIDOID: type = PDO_PARAM_LOB; break;
        case PG_FLOAT4OID: native = "float4"; break;
        case PG_FLOAT8OID: native = "float8"; break;
        case PG_TEXTOID: native = "text"; break;
        case PG_VARCHAROID: native = "varchar"; break;
        case PG_DATEOID: native = "date"; break;
        case PG_TIMESTAMPOID: native = "timestamp"; break;
        default: break;
    }
    if (native != NULL) {
        add_assoc_string(return_value, "native_type", native);
    }
    add_assoc_long(return_value, "pdo_type", type);
    return SUCCESS;
}

static int pg_stmt_cursor_closer(pdo_stmt_t *stmt) {
    pg_free_result(stmt->driver_data);
    return 1;
}

static const struct pdo_stmt_methods tenma_pg_stmt_methods = {
    pg_stmt_dtor,
    pg_stmt_execute,
    pg_stmt_fetch,
    pg_stmt_describe,
    pg_stmt_get_col,
    pg_stmt_param_hook,
    NULL, /* set_attribute */
    NULL, /* get_attribute */
    pg_stmt_get_column_meta,
    NULL, /* next_rowset */
    pg_stmt_cursor_closer,
};

/** user='...' for the connection string: quotes and backslashes escaped, as pdo_pgsql does. */
static void pg_append_credential(smart_str *out, const char *key, const char *value) {
    smart_str_appendc(out, ' ');
    smart_str_appends(out, key);
    smart_str_appendl(out, "='", 2);
    for (const char *p = value; *p != '\0'; p++) {
        if (*p == '\'' || *p == '\\') {
            smart_str_appendc(out, '\\');
        }
        smart_str_appendc(out, *p);
    }
    smart_str_appendc(out, '\'');
}

static int pg_handle_factory(pdo_dbh_t *dbh, zval *driver_options) {
    tenma_pg_handle *H = pecalloc(1, sizeof(tenma_pg_handle), dbh->is_persistent);
    H->persistent = dbh->is_persistent;
    dbh->driver_data = H;
    dbh->skip_param_evt = 1 << PDO_PARAM_EVT_EXEC_POST | 1 << PDO_PARAM_EVT_FETCH_PRE | 1 << PDO_PARAM_EVT_FETCH_POST;
    // PDO's "key=value;key=value" as libpq's "key=value key=value"
    smart_str conninfo = {0};
    for (size_t i = 0; i < dbh->data_source_len; i++) {
        smart_str_appendc(&conninfo, dbh->data_source[i] == ';' ? ' ' : dbh->data_source[i]);
    }
    zend_long timeout = driver_options != NULL ? pdo_attr_lval(driver_options, PDO_ATTR_TIMEOUT, 30) : 30;
    smart_str_append_printf(&conninfo, " connect_timeout=" ZEND_LONG_FMT, timeout);
    if (dbh->username != NULL && strstr(dbh->data_source, "user=") == NULL) {
        pg_append_credential(&conninfo, "user", dbh->username);
    }
    if (dbh->password != NULL && strstr(dbh->data_source, "password=") == NULL) {
        pg_append_credential(&conninfo, "password", dbh->password);
    }
    smart_str_0(&conninfo);
    zend_string *buffer;
    int ok = pg_call(dbh, NULL, "connect", ZSTR_VAL(conninfo.s), ZSTR_LEN(conninfo.s), NULL, 0, &buffer);
    smart_str_free(&conninfo);
    // From here on PDO calls the closer when the object goes
    dbh->methods = &tenma_pg_methods;
    if (!ok) {
        return 0;
    }
    H->id = ZEND_STRTOUL(ZSTR_VAL(buffer), NULL, 10);
    zend_string_release(buffer);
    dbh->alloc_own_columns = 1;
    dbh->max_escaped_char_length = 2;
    return 1;
}

static const pdo_driver_t tenma_pg_driver = {
    PDO_DRIVER_HEADER(pgsql),
    pg_handle_factory,
};

/** Puts tenma's driver in the place of pdo_pgsql, unless --pdo-pgsql native keeps libpq. */
static void register_pdo_driver(void) {
    if (pdo_native) {
        return;
    }
    php_pdo_unregister_driver(&pdo_pgsql_driver);
    php_pdo_register_driver(&tenma_pg_driver);
}

/* ---------------------------------------------------------------------------------------------------
 * The journal (src/journal.rs): PHP functions whose results come from the outside world (the clock,
 * randomness, the process) are wrapped. Recording writes every result into the journal, a replay
 * returns the recorded one instead of calling the function. Functions that block the worker are
 * noted, so a replay can say why it cannot be exact.
 * ------------------------------------------------------------------------------------------------- */

#include <ext/standard/php_var.h>

extern void tenma_rs_journal_value(const char *name, size_t name_len, const char *value, size_t len);
extern int tenma_rs_journal_take(const char *name, size_t name_len, char **value, size_t *len);
extern void tenma_rs_journal_free(char *value, size_t len);
extern void tenma_rs_journal_note(const char *name, size_t name_len);

/** 0: nothing. Otherwise the wrapped functions report to src/journal.rs, which records, replays or keeps for a durable function. */
static _Thread_local int journal_mode = 0;

void tenma_set_journal_mode(int mode) {
    journal_mode = mode;
}

/** The request whose code runs right now, Tenma\Internal\Current::$request; 0 for none (startup, timers, detached tasks). */
uint64_t tenma_current_request(void) {
    zend_class_entry *ce = zend_hash_str_find_ptr(EG(class_table), ZEND_STRL("tenma\\internal\\current"));
    if (ce == NULL) {
        return 0;
    }
    zval *request = zend_read_static_property(ce, ZEND_STRL("request"), true);
    return request != NULL && Z_TYPE_P(request) == IS_LONG ? (uint64_t) Z_LVAL_P(request) : 0;
}

static struct {
    const char *name;
    /** 1: the function blocks the worker; only noted, never replaced */
    int blocking;
    zif_handler original;
} journaled[] = {
    {"time", 0, NULL}, {"microtime", 0, NULL}, {"hrtime", 0, NULL}, {"date", 0, NULL}, {"gmdate", 0, NULL},
    {"idate", 0, NULL}, {"localtime", 0, NULL}, {"getdate", 0, NULL}, {"mktime", 0, NULL}, {"gmmktime", 0, NULL},
    {"strtotime", 0, NULL}, {"date_create", 0, NULL}, {"date_create_immutable", 0, NULL},
    {"random_int", 0, NULL}, {"random_bytes", 0, NULL}, {"mt_rand", 0, NULL}, {"rand", 0, NULL},
    {"lcg_value", 0, NULL}, {"uniqid", 0, NULL}, {"getmypid", 0, NULL}, {"gethostname", 0, NULL},
    {"sys_getloadavg", 0, NULL},
    {"sleep", 1, NULL}, {"usleep", 1, NULL}, {"time_nanosleep", 1, NULL}, {"curl_exec", 1, NULL},
    {"curl_multi_exec", 1, NULL}, {"pg_connect", 1, NULL}, {"pg_query", 1, NULL},
};

static ZEND_NAMED_FUNCTION(journaled_function) {
    zend_string *name = execute_data->func->common.function_name;
    zif_handler original = NULL;
    int blocking = 0;
    for (size_t i = 0; i < sizeof(journaled) / sizeof(journaled[0]); i++) {
        if (zend_string_equals_cstr(name, journaled[i].name, strlen(journaled[i].name))) {
            original = journaled[i].original;
            blocking = journaled[i].blocking;
        }
    }
    if (original == NULL) {
        zend_throw_error(NULL, "tenma: journaled function %s has no original", ZSTR_VAL(name));
        return;
    }
    if (journal_mode != 0 && blocking) {
        tenma_rs_journal_note(ZSTR_VAL(name), ZSTR_LEN(name));
    }
    if (journal_mode != 0 && !blocking) {
        char *value;
        size_t len;
        if (tenma_rs_journal_take(ZSTR_VAL(name), ZSTR_LEN(name), &value, &len)) {
            const unsigned char *p = (const unsigned char *) value;
            // A journal may come from a database others can write: only the class the function
            // returns, so a stored object cannot run code of its own
            HashTable allowed;
            zend_hash_init(&allowed, 1, NULL, NULL, 0);
            if (zend_string_equals_literal(name, "date_create")) {
                zend_hash_str_add_empty_element(&allowed, ZEND_STRL("datetime"));
            } else if (zend_string_equals_literal(name, "date_create_immutable")) {
                zend_hash_str_add_empty_element(&allowed, ZEND_STRL("datetimeimmutable"));
            }
            php_unserialize_data_t var_hash;
            PHP_VAR_UNSERIALIZE_INIT(var_hash);
            php_var_unserialize_set_allowed_classes(var_hash, &allowed);
            int ok = php_var_unserialize(return_value, &p, p + len, &var_hash);
            PHP_VAR_UNSERIALIZE_DESTROY(var_hash);
            zend_hash_destroy(&allowed);
            tenma_rs_journal_free(value, len);
            // Any other class arrives as __PHP_Incomplete_Class: not a value the function returns
            if (ok && Z_TYPE_P(return_value) == IS_OBJECT
                && zend_string_equals_literal_ci(Z_OBJCE_P(return_value)->name, "__PHP_Incomplete_Class")) {
                ok = 0;
            }
            if (ok) {
                return;
            }
            zval_ptr_dtor(return_value);
            ZVAL_UNDEF(return_value);
        }
    }
    original(INTERNAL_FUNCTION_PARAM_PASSTHRU);
    if (journal_mode != 0 && !blocking && EG(exception) == NULL) {
        smart_str buffer = {0};
        php_serialize_data_t data;
        PHP_VAR_SERIALIZE_INIT(data);
        php_var_serialize(&buffer, return_value, &data);
        PHP_VAR_SERIALIZE_DESTROY(data);
        tenma_rs_journal_value(ZSTR_VAL(name), ZSTR_LEN(name), buffer.s != NULL ? ZSTR_VAL(buffer.s) : "", buffer.s != NULL ? ZSTR_LEN(buffer.s) : 0);
        smart_str_free(&buffer);
    }
}

static void wrap_journaled(void) {
    for (size_t i = 0; i < sizeof(journaled) / sizeof(journaled[0]); i++) {
        zend_function *function = zend_hash_str_find_ptr(CG(function_table), journaled[i].name, strlen(journaled[i].name));
        if (function != NULL && function->type == ZEND_INTERNAL_FUNCTION) {
            journaled[i].original = function->internal_function.handler;
            function->internal_function.handler = journaled_function;
        }
    }
}

int tenma_start(void) {
    // Like console.log, echo reaches the console at once, even when stdout is a pipe
    setvbuf(stdout, NULL, _IOLBF, 0);
    php_embed_module.startup = tenma_startup;
    php_embed_module.read_post = tenma_read_post;
    embed_ub_write = php_embed_module.ub_write;
    php_embed_module.ub_write = tenma_ub_write;
    php_embed_module.ini_defaults = tenma_ini_defaults;
    if (inspect_mode) {
        embed_log_message = php_embed_module.log_message;
        php_embed_module.log_message = tenma_log_message;
    }
    php_embed_module.name = "tenma";
    php_embed_module.pretty_name = "tenma";
    if (php_embed_init(0, NULL) != SUCCESS) {
        return -1;
    }
    release_signals();
    wrap_exit();
    register_async_transports();
    wrap_stream_select();
    wrap_session_start();
    register_pdo_driver();
    wrap_journaled();
    return 0;
}

void tenma_stop(void) {
    php_embed_shutdown();
}

/**
 * STDIN, STDOUT and STDERR as in the PHP CLI: constants holding streams on the process's file
 * descriptors. Outside the CLI they are dup() copies; PHP closes them with the interpreter, the
 * process's own stay open. (The CLI keeps them open because it uses the originals.)
 */
static void register_std_streams(void) {
    const char *names[] = {"STDIN", "STDOUT", "STDERR"};
    const char *paths[] = {"php://stdin", "php://stdout", "php://stderr"};
    const char *modes[] = {"rb", "wb", "wb"};
    for (int i = 0; i < 3; i++) {
        php_stream *stream = php_stream_open_wrapper_ex(paths[i], modes[i], 0, NULL, NULL);
        if (!stream) {
            continue;
        }
        zend_constant constant;
        php_stream_to_zval(stream, &constant.value);
        Z_CONSTANT_FLAGS(constant.value) = 0;
        constant.name = zend_string_init_interned(names[i], strlen(names[i]), 0);
        zend_register_constant(&constant);
    }
}

/**
 * Turns the calling thread into a worker with its own PHP interpreter (ZTS): its own globals,
 * its own request as after php_embed_init. tenma_start must have run in the main thread first.
 */
int tenma_thread_start(void) {
    (void) ts_resource(0);
    // A fatal error inside tenma_parse_multipart skips its reset: the body it points at belonged to
    // the old interpreter, and php://input of the next request would read it
    post_data = NULL;
    post_len = post_pos = 0;
    SIGG(reset) = 0;
    SG(options) |= SAPI_OPTION_NO_CHDIR;
    if (php_request_startup() == FAILURE) {
        ts_free_thread();
        return -1;
    }
    SG(headers_sent) = 1;
    SG(request_info).no_headers = 1;
    php_register_variable("PHP_SELF", "-", NULL);
    register_std_streams();
    return 0;
}

void tenma_thread_stop(void) {
    classic_open = 0;
    memset(&classic_output, 0, sizeof(classic_output));
    ini_snapshot = NULL;
    outer_shutdown_functions = NULL;
    // The handlers may own the app (new Http\Server(8080, new App())): its destructor is user code, and a
    // bailout there without zend_try would exit the whole process instead of ending this interpreter
    zend_try {
        for (uint32_t i = 0; i < handler_count; i++) {
            zval_ptr_dtor(&handlers[i]);
        }
    } zend_end_try();
    handler_count = 0;
    php_request_shutdown(NULL);
    if (output_states) {
        // What fibers still held went with the request's memory; only the records are left
        output_state *state;
        ZEND_HASH_FOREACH_PTR(output_states, state) {
            pefree(state, 1);
        } ZEND_HASH_FOREACH_END();
        zend_hash_destroy(output_states);
        pefree(output_states, 1);
        output_states = NULL;
    }
    ts_free_thread();
}

/**
 * tenma --watch: reports every file PHP loaded from position `from` on to Rust and returns the
 * new count. EG(included_files) only grows, and PHP adds a file before compiling it, so a file
 * with a syntax error is watched too.
 */
size_t tenma_report_included(size_t from) {
    size_t count = zend_hash_num_elements(&EG(included_files));
    if (count <= from) {
        return count;
    }
    size_t index = 0;
    zend_string *path;
    ZEND_HASH_FOREACH_STR_KEY(&EG(included_files), path) {
        if (index++ >= from && path) {
            tenma_rs_included(ZSTR_VAL(path), ZSTR_LEN(path));
        }
    } ZEND_HASH_FOREACH_END();
    return count;
}

/**
 * tenma --inspect: code this thread compiles from now on stops the debugger before each statement.
 * Not eval()'d code like the stdlib: PHP compiles that without the statement opcodes.
 */
void tenma_inspect_thread(void) {
    CG(compiler_options) |= ZEND_COMPILE_EXTENDED_STMT;
    inspecting = 1;
    // A fatal error in a destructor during tenma_inspect_resume left it set
    evaluating = 0;
}

/**
 * DAP threads are the fibers: thread 1 is the code outside fibers, a fiber is its object handle
 * plus 1. The fiber with this ID, NULL for thread 1 or if there is none.
 */
static zend_fiber *fiber_of(uint32_t thread) {
    uint32_t handle = thread - 1;
    if (thread < 2 || handle >= EG(objects_store).top) {
        return NULL;
    }
    zend_object *object = EG(objects_store).object_buckets[handle];
    return IS_OBJ_VALID(object) && object->ce == zend_ce_fiber ? (zend_fiber *) object : NULL;
}

static uint32_t thread_of(zend_fiber *fiber) {
    return fiber ? fiber->std.handle + 1 : 1;
}

/** The thread the debugger stopped in. */
uint32_t tenma_inspect_thread_id(void) {
    return thread_of(EG(active_fiber));
}

/**
 * The innermost frame of a thread and its first: the stopped one from the statement it stopped at,
 * a waiting fiber from where it suspended. Below a fiber's first frame lies whoever resumed it,
 * which changes from resume to resume. 0 if the thread has no frames to show.
 */
static int thread_frames(uint32_t thread, zend_execute_data **top, zend_execute_data **bottom) {
    zend_fiber *fiber = fiber_of(thread);
    if (fiber == EG(active_fiber) && (fiber || thread == 1)) {
        *top = paused;
    } else if (fiber && fiber->context.status == ZEND_FIBER_STATUS_SUSPENDED) {
        *top = fiber->execute_data;
    } else {
        return 0;
    }
    *bottom = fiber ? fiber->stack_bottom : NULL;
    return 1;
}

#define FOREACH_FRAME(top, bottom, frame) \
    for (zend_execute_data *frame = (top); frame; frame = frame == (bottom) ? NULL : frame->prev_execute_data)

/** Frames shown in the editor: PHP code, not internal functions and not the stdlib. */
static int shown_frame(zend_execute_data *frame) {
    return frame->func && ZEND_USER_CODE(frame->func->type) && frame->opline
        && strncmp(ZSTR_VAL(frame->func->op_array.filename), "tenma:", 6) != 0;
}

/** Copies packed fields into malloc() memory, which Rust can free on its own. */
static char *malloc_copy(smart_str *out, size_t *len) {
    zend_string *fields = smart_str_extract(out);
    *len = ZSTR_LEN(fields);
    char *copy = malloc(*len + 1);
    memcpy(copy, ZSTR_VAL(fields), *len);
    zend_string_release(fields);
    return copy;
}

/** A field ends with a null byte: one inside (an array key, a string) is shown as \0. */
static void add_field(smart_str *out, const char *text, size_t len) {
    for (const char *nul; (nul = memchr(text, 0, len)); len -= nul - text + 1, text = nul + 1) {
        smart_str_appendl(out, text, nul - text);
        smart_str_appends(out, "\\0");
    }
    smart_str_appendl(out, text, len);
    smart_str_appendc(out, 0);
}

/** How deep the stopped statement is in its fiber: a step over or out compares this. */
uint32_t tenma_inspect_depth(void) {
    uint32_t depth = 0;
    FOREACH_FRAME(paused, EG(active_fiber) ? EG(active_fiber)->stack_bottom : NULL, frame) {
        depth++;
    }
    return depth;
}

/**
 * The threads as "id\0name\0" each, the stopped one first: the tasks of Tenma's scheduler (requests,
 * timers, Task::run()) under their label, waiting ones marked. Rust frees it with free().
 */
char *tenma_inspect_threads(size_t *len) {
    smart_str out = {0};
    zend_fiber *stopped = EG(active_fiber);
    smart_str_append_unsigned(&out, thread_of(stopped));
    smart_str_appendc(&out, 0);
    zend_class_entry *scheduler = zend_hash_str_find_ptr(EG(class_table), ZEND_STRL("tenma\\internal\\scheduler"));
    zval *tasks = scheduler ? zend_read_static_property(scheduler, ZEND_STRL("taskOfFiber"), 1) : NULL;
    zval *stopped_task = tasks && Z_TYPE_P(tasks) == IS_ARRAY && stopped ? zend_hash_index_find(Z_ARRVAL_P(tasks), stopped->std.handle) : NULL;
    zval label;
    if (stopped_task) {
        zval *name = zend_read_property(Z_OBJCE_P(stopped_task), Z_OBJ_P(stopped_task), ZEND_STRL("label"), 1, &label);
        add_field(&out, Z_TYPE_P(name) == IS_STRING ? Z_STRVAL_P(name) : "Task", Z_TYPE_P(name) == IS_STRING ? Z_STRLEN_P(name) : 4);
    } else {
        smart_str_appends(&out, stopped ? "Fiber" : "main");
        smart_str_appendc(&out, 0);
    }
    if (tasks && Z_TYPE_P(tasks) == IS_ARRAY) {
        zend_ulong handle;
        zval *task;
        ZEND_HASH_FOREACH_NUM_KEY_VAL(Z_ARRVAL_P(tasks), handle, task) {
            zend_fiber *fiber = fiber_of(handle + 1);
            if (!fiber || fiber == stopped || Z_TYPE_P(task) != IS_OBJECT) {
                continue;
            }
            smart_str_append_unsigned(&out, handle + 1);
            smart_str_appendc(&out, 0);
            zval *name = zend_read_property(Z_OBJCE_P(task), Z_OBJ_P(task), ZEND_STRL("label"), 1, &label);
            if (Z_TYPE_P(name) == IS_STRING) {
                smart_str_append(&out, Z_STR_P(name));
            }
            if (fiber->context.status == ZEND_FIBER_STATUS_SUSPENDED) {
                smart_str_appends(&out, " (waiting)");
            }
            smart_str_appendc(&out, 0);
        } ZEND_HASH_FOREACH_END();
    }
    return malloc_copy(&out, len);
}

/** The shown frames of a thread as "file\0line\0function\0" each; Rust frees it with free(). */
char *tenma_inspect_frames(uint32_t thread, size_t *len) {
    smart_str out = {0};
    zend_execute_data *top, *bottom;
    if (!thread_frames(thread, &top, &bottom)) {
        return malloc_copy(&out, len);
    }
    FOREACH_FRAME(top, bottom, frame) {
        if (!shown_frame(frame)) {
            continue;
        }
        zend_function *function = frame->func;
        add_field(&out, ZSTR_VAL(function->op_array.filename), ZSTR_LEN(function->op_array.filename));
        smart_str_append_unsigned(&out, frame == paused && paused_line ? paused_line : frame->opline->lineno);
        smart_str_appendc(&out, 0);
        if (!function->common.function_name) {
            smart_str_appends(&out, "{main}");
        } else {
            if (function->common.scope) {
                smart_str_append(&out, function->common.scope->name);
                smart_str_appends(&out, "::");
            }
            smart_str_append(&out, function->common.function_name);
        }
        smart_str_appendc(&out, 0);
    }
    return malloc_copy(&out, len);
}

/** An object's properties without running PHP code (no __debugInfo, no lazy initializer); NULL if not. */
static HashTable *properties_of(zend_object *object) {
    return zend_object_is_lazy(object) ? NULL : object->handlers->get_properties(object);
}

/**
 * An array or object the editor may expand while stopped; its reference to one is the position in
 * expandable[] plus 1. Where it lies: `parent` and `key` or `index`; `table` and `key` for a
 * variable in a symbol table (its buckets move when evaluated code adds variables); or `value` for
 * a local (a slot that stays put while stopped). Elements are looked up again on each use: evaluated
 * code may have changed or freed what was there. Changing elements copies shared arrays on the way,
 * as PHP would.
 */
struct expandable {
    zval *value;
    uint32_t parent;
    zend_string *key;
    zend_ulong index;
    HashTable *table;
};

/**
 * Where a variable lies: in the array or object `parent` refers to, under `key` or `index`; in the
 * symbol table `table` under `key`; NULL for a local.
 */
struct place {
    uint32_t parent;
    zend_string *key;
    zend_ulong index;
    HashTable *table;
};

static _Thread_local struct expandable *expandable = NULL;
static _Thread_local uint32_t expandable_count = 0, expandable_size = 0;
/** Results of evaluated code, kept until the worker runs on: the editor may expand them. */
static _Thread_local zval **evaluated = NULL;
static _Thread_local uint32_t evaluated_count = 0;

static uint32_t expandable_reference(zval *value, const struct place *place) {
    if (expandable_count == expandable_size) {
        expandable_size = expandable_size ? expandable_size * 2 : 64;
        expandable = realloc(expandable, expandable_size * sizeof(*expandable));
    }
    struct expandable *entry = &expandable[expandable_count++];
    *entry = (struct expandable) {value, place ? place->parent : 0, place && place->key ? zend_string_copy(place->key) : NULL, place ? place->index : 0, place ? place->table : NULL};
    return expandable_count;
}

/**
 * The storage of an expandable value as it is now; NULL if it is gone. `write`: ready to be changed,
 * every shared array on its way from the variable is copied first (copy on write), so other copies
 * keep their value.
 */
static zval *lookup(uint32_t reference, bool write) {
    if (reference == 0 || reference > expandable_count) {
        return NULL;
    }
    struct expandable *entry = &expandable[reference - 1];
    if (entry->table) {
        zval *value = zend_hash_find(entry->table, entry->key);
        if (value) {
            ZVAL_DEINDIRECT(value);
            ZVAL_DEREF(value);
        }
        return value;
    }
    if (!entry->parent) {
        zval *value = entry->value;
        ZVAL_DEREF(value);
        return value;
    }
    zval *container = lookup(entry->parent, write);
    if (!container) {
        return NULL;
    }
    ZVAL_DEREF(container);
    zval *slot = NULL;
    if (Z_TYPE_P(container) == IS_ARRAY) {
        if (write) {
            SEPARATE_ARRAY(container);
        }
        slot = entry->key ? zend_hash_find(Z_ARRVAL_P(container), entry->key) : zend_hash_index_find(Z_ARRVAL_P(container), entry->index);
    } else if (Z_TYPE_P(container) == IS_OBJECT) {
        HashTable *properties = properties_of(Z_OBJ_P(container));
        slot = !properties ? NULL : entry->key ? zend_hash_find(properties, entry->key) : zend_hash_index_find(properties, entry->index);
    }
    if (slot) {
        ZVAL_DEINDIRECT(slot);
        ZVAL_DEREF(slot);
    }
    return slot;
}

/** The worker runs on: the references to arrays and objects lose their meaning. */
void tenma_inspect_resume(void) {
    for (uint32_t i = 0; i < expandable_count; i++) {
        if (expandable[i].key) {
            zend_string_release(expandable[i].key);
        }
    }
    free(expandable);
    expandable = NULL;
    expandable_count = expandable_size = 0;
    // Releasing them may run destructors: those must not stop the debugger while it resumes
    evaluating = 1;
    zend_fiber_switch_block();
    for (uint32_t i = 0; i < evaluated_count; i++) {
        zval_ptr_dtor(evaluated[i]);
        free(evaluated[i]);
    }
    zend_fiber_switch_unblock();
    evaluating = 0;
    free(evaluated);
    evaluated = NULL;
    evaluated_count = 0;
}


/** A value as the editor shows it: scalars in full (long strings cut), arrays and objects briefly. */
static void describe(smart_str *out, zval *value) {
    switch (Z_TYPE_P(value)) {
    case IS_ARRAY:
        smart_str_append_printf(out, "array(%u)", zend_hash_num_elements(Z_ARRVAL_P(value)));
        break;
    case IS_OBJECT:
        smart_str_append(out, Z_OBJCE_P(value)->name);
        if (Z_OBJCE_P(value)->ce_flags & ZEND_ACC_ENUM) {
            smart_str_appends(out, "::");
            smart_str_append(out, Z_STR_P(zend_enum_fetch_case_name(Z_OBJ_P(value))));
        }
        break;
    case IS_RESOURCE:
        smart_str_append_printf(out, "resource(%d)", Z_RES_HANDLE_P(value));
        break;
    default:
        smart_str_append_scalar(out, value, 1000);
    }
}

/** A variable as "name\0value\0type\0reference\0"; the reference is 0 for values without children. */
static void add_variable(smart_str *out, const char *prefix, const char *name, size_t name_len, zval *value, const struct place *place) {
    ZVAL_DEINDIRECT(value);
    // A variable's slot, not the reference it may hold: evaluated code may free that reference
    zval *slot = value;
    ZVAL_DEREF(value);
    if (Z_TYPE_P(value) == IS_UNDEF) {
        return;
    }
    smart_str_appends(out, prefix);
    add_field(out, name, name_len);
    describe(out, value);
    smart_str_appendc(out, 0);
    smart_str_appends(out, zend_zval_type_name(value));
    smart_str_appendc(out, 0);
    int children = Z_TYPE_P(value) == IS_ARRAY ? zend_hash_num_elements(Z_ARRVAL_P(value)) > 0
        : Z_TYPE_P(value) == IS_OBJECT && !(Z_OBJCE_P(value)->ce_flags & ZEND_ACC_ENUM);
    smart_str_append_unsigned(out, children ? expandable_reference(slot, place) : 0);
    smart_str_appendc(out, 0);
}

/** The local variables of a thread's shown frame `index`, as add_variable packs them. */
char *tenma_inspect_locals(uint32_t thread, uint32_t index, size_t *len) {
    smart_str out = {0};
    zend_execute_data *top, *bottom;
    if (!thread_frames(thread, &top, &bottom)) {
        return malloc_copy(&out, len);
    }
    FOREACH_FRAME(top, bottom, frame) {
        if (!shown_frame(frame) || index-- > 0) {
            continue;
        }
        if (frame == paused && paused_return) {
            add_variable(&out, "", ZEND_STRL("return value"), paused_return, NULL);
        }
        if (Z_TYPE(frame->This) == IS_OBJECT) {
            add_variable(&out, "$", ZEND_STRL("this"), &frame->This, NULL);
        }
        if (ZEND_CALL_INFO(frame) & ZEND_CALL_HAS_SYMBOL_TABLE) {
            // The script's top level: also variables created at runtime ($$name, extract()), without superglobals
            zend_string *name;
            zval *value;
            ZEND_HASH_FOREACH_STR_KEY_VAL(frame->symbol_table, name, value) {
                if (name && !zend_is_auto_global(name)) {
                    struct place place = {0, name, 0, frame->symbol_table};
                    add_variable(&out, "$", ZSTR_VAL(name), ZSTR_LEN(name), value, &place);
                }
            } ZEND_HASH_FOREACH_END();
        } else {
            zend_op_array *op_array = &frame->func->op_array;
            for (int i = 0; i < op_array->last_var; i++) {
                add_variable(&out, "$", ZSTR_VAL(op_array->vars[i]), ZSTR_LEN(op_array->vars[i]), ZEND_CALL_VAR_NUM(frame, i), NULL);
            }
        }
        break;
    }
    return malloc_copy(&out, len);
}

/** The elements of an array or the properties of an object the editor expands, as add_variable packs them. */
char *tenma_inspect_children(uint32_t reference, size_t *len) {
    smart_str out = {0};
    zval *value = lookup(reference, false);
    HashTable *children = !value ? NULL : Z_TYPE_P(value) == IS_ARRAY ? Z_ARRVAL_P(value) : Z_TYPE_P(value) == IS_OBJECT ? properties_of(Z_OBJ_P(value)) : NULL;
    if (!children) {
        return malloc_copy(&out, len);
    }
    // ponytail: the first 1000 only; DAP paging (start, count) if larger ones matter
    uint32_t shown = 0;
    zend_ulong index;
    zend_string *key;
    zval *child;
    ZEND_HASH_FOREACH_KEY_VAL(children, index, key, child) {
        if (shown++ == 1000) {
            break;
        }
        struct place place = {reference, key, index};
        if (!key) {
            char number[24];
            add_variable(&out, "", number, snprintf(number, sizeof(number), ZEND_ULONG_FMT, index), child, &place);
        } else if (Z_TYPE_P(value) == IS_OBJECT) {
            // Private and protected properties are stored as "\0Class\0name" and "\0*\0name"
            const char *class_name, *property;
            size_t property_len;
            zend_unmangle_property_name_ex(key, &class_name, &property, &property_len);
            add_variable(&out, "", property, property_len, child, &place);
        } else {
            add_variable(&out, "", ZSTR_VAL(key), ZSTR_LEN(key), child, &place);
        }
    } ZEND_HASH_FOREACH_END();
    return malloc_copy(&out, len);
}

/** The message of an exception, NULL if it has none. */
static zend_string *exception_message(zend_object *exception) {
    zval value;
    zval *message = zend_read_property(zend_get_exception_base(exception), exception, ZEND_STRL("message"), 1, &value);
    return Z_TYPE_P(message) == IS_STRING ? Z_STR_P(message) : NULL;
}

/**
 * Running code while stopped, in a frame: eval() uses the variables of EG(current_execute_data).
 * Stopped at a throw, the exception is pending and waits until the code ran; a throw and
 * zend_clear_exception() move the frames' opline, which the VM reads on after the stop.
 */
struct in_frame {
    zend_execute_data *current, *target;
    const zend_op *current_opline, *target_opline, *before_exception;
    zend_object *pending;
};

static void enter_frame(struct in_frame *run, zend_execute_data *target) {
    *run = (struct in_frame) {EG(current_execute_data), target, EG(current_execute_data)->opline, target->opline, EG(opline_before_exception), EG(exception)};
    EG(exception) = NULL;
    EG(current_execute_data) = target;
    evaluating = 1;
    // A fiber switch (Task::delay() in the code) would leave the stopped statement mid-way
    zend_fiber_switch_block();
}

static void leave_frame(struct in_frame *run) {
    zend_fiber_switch_unblock();
    evaluating = 0;
    EG(current_execute_data) = run->current;
    run->current->opline = run->current_opline;
    run->target->opline = run->target_opline;
    EG(opline_before_exception) = run->before_exception;
    EG(exception) = run->pending;
}

/** Packs "error\0message\0" if the code threw (or `failure`), and clears the exception. */
static int failed(smart_str *out, bool failure) {
    if (!EG(exception) && !failure) {
        return 0;
    }
    smart_str_appends(out, "error");
    smart_str_appendc(out, 0);
    if (EG(exception)) {
        zend_string *message = exception_message(EG(exception));
        smart_str_append(out, EG(exception)->ce->name);
        if (message && ZSTR_LEN(message)) {
            smart_str_appends(out, ": ");
            smart_str_append(out, message);
        }
        zend_clear_exception();
    } else {
        smart_str_appends(out, "could not evaluate");
    }
    smart_str_appendc(out, 0);
    return 1;
}

/** Evaluates code in the entered frame into `result` (NULL without a result); otherwise packs the error. */
static int evaluate_code(smart_str *out, const char *code, size_t code_len, bool statements, zval *result) {
    ZVAL_NULL(result);
    zend_result compiled = FAILURE;
    bool fatal = false;
    // Warnings and notices are the evaluation's answer, not output on the console
    bool recording = EG(record_errors);
    uint32_t num_errors = EG(num_errors);
    zend_error_info **errors = EG(errors);
    EG(num_errors) = 0;
    EG(errors) = NULL;
    EG(record_errors) = true;
    zend_try {
        compiled = zend_eval_stringl(code, code_len, statements ? NULL : result, "debugger");
    } zend_catch {
        fatal = true;
    } zend_end_try();
    zend_error_info *error = EG(num_errors) ? EG(errors)[0] : NULL;
    bool warned = error && !fatal && !EG(exception) && compiled == SUCCESS;
    if (warned) {
        int type = error->type;
        smart_str_appends(out, "error");
        smart_str_appendc(out, 0);
        smart_str_appends(out, type & (E_NOTICE | E_USER_NOTICE) ? "Notice: " : type & (E_DEPRECATED | E_USER_DEPRECATED) ? "Deprecated: " : "Warning: ");
        smart_str_append(out, error->message);
        smart_str_appendc(out, 0);
    }
    zend_free_recorded_errors();
    EG(record_errors) = recording;
    EG(num_errors) = num_errors;
    EG(errors) = errors;
    if (warned) {
        zval_ptr_dtor(result);
        return -1;
    }
    if (fatal) {
        evaluation_fatal = 1;
        smart_str_appends(out, "fatal");
        smart_str_appendc(out, 0);
        if (PG(last_error_message)) {
            smart_str_append(out, PG(last_error_message));
        }
        smart_str_appendc(out, 0);
        return -1;
    }
    if (failed(out, compiled == FAILURE)) {
        zval_ptr_dtor(result);
        return -1;
    }
    if (Z_TYPE_P(result) == IS_UNDEF) {
        ZVAL_NULL(result);
    }
    return 0;
}

/** Packs "ok\0" and a value as add_variable does, then "truthy\0text\0"; keeps a copy to expand. */
static void pack_result(smart_str *out, zval *value) {
    smart_str_appends(out, "ok");
    smart_str_appendc(out, 0);
    // Kept where it does not move: expandable[] may point at it
    zval *kept = malloc(sizeof(zval));
    ZVAL_COPY_DEREF(kept, value);
    evaluated = realloc(evaluated, ++evaluated_count * sizeof(*evaluated));
    evaluated[evaluated_count - 1] = kept;
    add_variable(out, "", "", 0, kept, NULL);
    smart_str_appendc(out, zend_is_true(kept) ? '1' : '0');
    smart_str_appendc(out, 0);
    if (Z_TYPE_P(kept) == IS_STRING) {
        add_field(out, Z_STRVAL_P(kept), Z_STRLEN_P(kept));
    } else {
        describe(out, kept);
        smart_str_appendc(out, 0);
    }
}

static zend_execute_data *shown_frame_at(uint32_t index) {
    FOREACH_FRAME(paused, EG(active_fiber) ? EG(active_fiber)->stack_bottom : NULL, frame) {
        if (shown_frame(frame) && index-- == 0) {
            return frame;
        }
    }
    return NULL;
}

static char *no_frame(smart_str *out, size_t *len) {
    smart_str_appends(out, "error");
    smart_str_appendc(out, 0);
    smart_str_appends(out, "not available in the stopped request");
    smart_str_appendc(out, 0);
    return malloc_copy(out, len);
}

/**
 * Evaluates `code` in shown frame `index` of the stopped thread, with its variables and $this, like
 * eval() there: an expression, or with `statements` code without a result. Packs
 * "status\0name\0value\0type\0reference\0truthy\0text\0" (see pack_result), or "error\0message\0"
 * or "fatal\0message\0". Rust frees it with free().
 */
char *tenma_inspect_evaluate(uint32_t index, const char *code, size_t code_len, bool statements, size_t *len) {
    smart_str out = {0};
    zend_execute_data *target = shown_frame_at(index);
    if (!target) {
        return no_frame(&out, len);
    }
    struct in_frame run;
    enter_frame(&run, target);
    zval result;
    if (evaluate_code(&out, code, code_len, statements, &result) == 0) {
        pack_result(&out, &result);
        zval_ptr_dtor(&result);
    }
    leave_frame(&run);
    return malloc_copy(&out, len);
}

/** The class whose scope may write property `name` of `object`, which may be private to a parent. */
static zend_class_entry *property_scope(zend_object *object, const char *name, size_t name_len) {
    HashTable *properties = properties_of(object);
    zend_string *key;
    if (properties) {
        ZEND_HASH_FOREACH_STR_KEY(properties, key) {
            const char *class_name, *property;
            size_t property_len;
            if (!key || zend_unmangle_property_name_ex(key, &class_name, &property, &property_len) != SUCCESS
                || property_len != name_len || memcmp(property, name, name_len) != 0) {
                continue;
            }
            if (class_name && class_name[0] != '*') {
                zend_class_entry *declaring = zend_hash_str_find_ptr_lc(EG(class_table), class_name, strlen(class_name));
                return declaring ? declaring : object->ce;
            }
            break;
        } ZEND_HASH_FOREACH_END();
    }
    return object->ce;
}

/**
 * Sets element or property `name` of the array or object `reference` refers to, to the value of
 * `code` (evaluated in the stopped frame). Typed and readonly properties check as in PHP. Packs the
 * new value as tenma_inspect_evaluate does.
 */
char *tenma_inspect_set_child(uint32_t reference, const char *name, size_t name_len, const char *code, size_t code_len, size_t *len) {
    smart_str out = {0};
    zend_execute_data *target = shown_frame_at(0);
    if (!target || reference == 0 || reference > expandable_count) {
        return no_frame(&out, len);
    }
    struct in_frame run;
    enter_frame(&run, target);
    zval value;
    if (evaluate_code(&out, code, code_len, false, &value) == 0) {
        zval *container = lookup(reference, true);
        if (container && Z_TYPE_P(container) == IS_ARRAY) {
            SEPARATE_ARRAY(container);
            pack_result(&out, zend_symtable_str_update(Z_ARRVAL_P(container), name, name_len, &value));
        } else if (container && Z_TYPE_P(container) == IS_OBJECT) {
            zend_object *object = Z_OBJ_P(container);
            zend_string *property = zend_string_init(name, name_len, 0);
            zend_class_entry *scope = property_scope(object, name, name_len);
            zend_update_property_ex(scope, object, property, &value);
            zval_ptr_dtor(&value);
            if (!failed(&out, false)) {
                zval read;
                pack_result(&out, zend_read_property_ex(scope, object, property, 1, &read));
            }
            zend_string_release(property);
        } else {
            zval_ptr_dtor(&value);
            failed(&out, true);
        }
    }
    leave_frame(&run);
    return malloc_copy(&out, len);
}

/** Debugger::break(): stops here if a debugger is connected. */
extern void tenma_rs_debugger(uintptr_t fiber);

PHP_FUNCTION(tenma_reloaded_by) {
    ZEND_PARSE_PARAMETERS_NONE();
    char buffer[4096];
    size_t len = tenma_rs_reloaded_by(buffer, sizeof buffer);
    if (len == SIZE_MAX) {
        RETURN_NULL();
    }
    if (len <= sizeof buffer) {
        RETURN_STRINGL(buffer, len);
    }
    // A longer path: once more with room for it
    zend_string *path = zend_string_alloc(len, 0);
    if (tenma_rs_reloaded_by(ZSTR_VAL(path), len) != len) {
        zend_string_release(path);
        RETURN_NULL();
    }
    ZSTR_VAL(path)[len] = '\0';
    RETURN_NEW_STR(path);
}

PHP_FUNCTION(tenma_debugger) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!inspecting || evaluating) {
        return;
    }
    zend_execute_data *outer = paused;
    paused = EG(current_execute_data);
    tenma_rs_debugger((uintptr_t) EG(active_fiber));
    paused = outer;
    bail_after_fatal_evaluation();
}

/** Where a thrown exception ends up, see exception_fate. */
enum fate { CAUGHT_INSIDE, CAUGHT, UNCAUGHT };

/**
 * Follows a thrown exception up the stopped fiber's frames: caught inside the stdlib before it
 * reaches PHP code of the app (Tenma's own business), caught by the app, or not: it leaves the app's
 * code into the stdlib (which turns it into a 500 or a task's error) or the fiber.
 */
static enum fate exception_fate(zend_object *exception) {
    int in_app = 0;
    FOREACH_FRAME(paused, EG(active_fiber) ? EG(active_fiber)->stack_bottom : NULL, frame) {
        if (!frame->func || !ZEND_USER_CODE(frame->func->type) || !frame->opline) {
            continue;
        }
        if (shown_frame(frame)) {
            in_app = 1;
        } else if (in_app) {
            return UNCAUGHT;
        }
        zend_op_array *op_array = &frame->func->op_array;
        uint32_t at = frame->opline - op_array->opcodes;
        for (int i = 0; i < op_array->last_try_catch; i++) {
            zend_try_catch_element *block = &op_array->try_catch_array[i];
            if (!block->catch_op || at < block->try_op || at >= block->catch_op) {
                continue;
            }
            // The catch clauses in order, each with the class it catches
            for (const zend_op *clause = &op_array->opcodes[block->catch_op]; clause->opcode == ZEND_CATCH; clause = OP_JMP_ADDR(clause, clause->op2)) {
                zend_class_entry *catches = zend_hash_find_ptr(EG(class_table), Z_STR_P(RT_CONSTANT(clause, clause->op1) + 1));
                if (catches && instanceof_function(exception->ce, catches)) {
                    return in_app ? CAUGHT : CAUGHT_INSIDE;
                }
                if (clause->extended_value & ZEND_LAST_CATCH) {
                    break;
                }
            }
        }
    }
    return in_app ? UNCAUGHT : CAUGHT_INSIDE;
}

extern void tenma_rs_exception(const char *description, size_t description_len, const char *classes, size_t classes_len, bool uncaught);


/** PHP throws: the frames are still alive here, so the debugger may stop with them. */
static void inspect_throw(zend_object *exception) {
    if (previous_throw_hook) {
        previous_throw_hook(exception);
    }
    // Threading\Cancelled ends a task, as Tenma intends; exit() and the end of a fiber (a reload ends waiting requests) are no exceptions
    if (!inspecting || evaluating || zend_string_equals_literal_ci(exception->ce->name, "Threading\\Cancelled")
        || zend_is_unwind_exit(exception) || zend_is_graceful_exit(exception)) {
        return;
    }
    zend_execute_data *outer = paused;
    paused = EG(current_execute_data);
    enum fate fate = exception_fate(exception);
    if (fate != CAUGHT_INSIDE) {
        zend_string *message = exception_message(exception);
        smart_str description = {0};
        smart_str_append(&description, exception->ce->name);
        if (message && ZSTR_LEN(message) > 0) {
            smart_str_appends(&description, ": ");
            smart_str_append(&description, message);
        }
        smart_str_0(&description);
        // The class, its parents and its interfaces, for exception breakpoints limited to classes
        smart_str classes = {0};
        for (zend_class_entry *class = exception->ce; class; class = class->parent) {
            add_field(&classes, ZSTR_VAL(class->name), ZSTR_LEN(class->name));
        }
        for (uint32_t i = 0; i < exception->ce->num_interfaces; i++) {
            add_field(&classes, ZSTR_VAL(exception->ce->interfaces[i]->name), ZSTR_LEN(exception->ce->interfaces[i]->name));
        }
        smart_str_0(&classes);
        tenma_rs_exception(ZSTR_VAL(description.s), ZSTR_LEN(description.s), ZSTR_VAL(classes.s), ZSTR_LEN(classes.s), fate == UNCAUGHT);
        smart_str_free(&classes);
        smart_str_free(&description);
    }
    paused = outer;
    bail_after_fatal_evaluation();
}

/** Prints an uncaught exception and clears it. */
static void report_exception(void) {
    if (EG(exception)) {
        zend_exception_error(EG(exception), E_WARNING);
    }
}

/** Runs PHP code without <?php, such as the bundled standard library. 0 on success. */
int tenma_eval(const char *code, size_t len, const char *name) {
    volatile int result = -1;
    zend_try {
        result = zend_eval_stringl((char *) code, len, NULL, name) == SUCCESS && !EG(exception) ? 0 : -1;
        report_exception();
    } zend_catch {
        EG(current_execute_data) = NULL;
    } zend_end_try();
    return result;
}

/**
 * Like the PHP CLI: $_SERVER knows the running script, $argv and $argc its arguments (argv[0] is
 * the script as given on the command line). Frameworks use this to find their files and tasks.
 */
static void register_script(const char *path, const char *const *argv, int argc) {
    zval args;
    array_init(&args);
    for (int i = 0; i < argc; i++) {
        add_next_index_string(&args, argv[i]);
    }
    zval count;
    ZVAL_LONG(&count, argc);
    zend_hash_str_update(&EG(symbol_table), ZEND_STRL("argv"), &args);
    zend_hash_str_update(&EG(symbol_table), ZEND_STRL("argc"), &count);

    zend_is_auto_global_str(ZEND_STRL("_SERVER"));
    zval *server = zend_hash_str_find(&EG(symbol_table), ZEND_STRL("_SERVER"));
    if (!server || Z_TYPE_P(server) != IS_ARRAY) {
        return;
    }
    SEPARATE_ARRAY(server);
    const char *keys[] = {"SCRIPT_FILENAME", "SCRIPT_NAME", "PHP_SELF", "PATH_TRANSLATED"};
    for (size_t i = 0; i < sizeof(keys) / sizeof(*keys); i++) {
        add_assoc_string(server, keys[i], path);
    }
    add_assoc_string(server, "DOCUMENT_ROOT", "");
    add_assoc_string(server, "SERVER_NAME", server_name);
    Z_TRY_ADDREF(args);
    add_assoc_zval(server, "argv", &args);
    add_assoc_long(server, "argc", argc);
}

/** Runs a PHP file (absolute path). Returns the exit status, 255 on a fatal error. */
int tenma_execute(const char *path, const char *const *argv, int argc) {
    script_exited = 0;
    register_script(path, argv, argc);
    zend_file_handle file;
    zend_stream_init_filename(&file, path);
    volatile int status = 255;
    zend_try {
        php_execute_script(&file);
        status = EG(exit_status);
    } zend_catch {
        EG(current_execute_data) = NULL;
    } zend_end_try();
    zend_destroy_file_handle(&file);
    return status;
}

static void add_str(zval *array, const char *key, tenma_str value) {
    add_assoc_stringl(array, key, value.ptr, value.len);
}

/**
 * The embed SAPI considers headers already sent. Before each request: no headers, status 200,
 * nothing sent. This makes header(), headers_list(), headers_sent() and
 * http_response_code() work as under FPM. no_headers stays set so PHP never sends by itself.
 */
static void reset_headers(void) {
    zend_llist_clean(&SG(sapi_headers).headers);
    SG(sapi_headers).http_response_code = 200;
    SG(headers_sent) = 0;
    // The output layer also remembers "already sent", e.g. from an echo at startup; otherwise
    // PHP warns as soon as header('Content-Length: …') tries to change zlib.output_compression
    OG(flags) &= ~PHP_OUTPUT_SENT;
}

/** Packs a list of strings as s\0s\0 … into a new buffer. */
static char *pack_lines(HashTable *lines, size_t *packed_len) {
    size_t length = 0;
    zval *line;
    // A null byte separates the lines: a line with its own null byte would start a foreign header
#define PACKABLE(line) (Z_TYPE_P(line) == IS_STRING && !memchr(Z_STRVAL_P(line), 0, Z_STRLEN_P(line)))
    ZEND_HASH_FOREACH_VAL(lines, line) {
        if (PACKABLE(line)) {
            length += Z_STRLEN_P(line) + 1;
        }
    } ZEND_HASH_FOREACH_END();
    char *packed = malloc(length ? length : 1);
    char *cursor = packed;
    ZEND_HASH_FOREACH_VAL(lines, line) {
        if (PACKABLE(line)) {
            memcpy(cursor, Z_STRVAL_P(line), Z_STRLEN_P(line) + 1);
            cursor += Z_STRLEN_P(line) + 1;
        }
    } ZEND_HASH_FOREACH_END();
#undef PACKABLE
    *packed_len = length;
    return packed;
}

/** Reads [int $status, list<string> $headers, string $body] from the standard library's return value. */
static int read_response(zval *value, tenma_response *response) {
    if (Z_TYPE_P(value) != IS_ARRAY) {
        return 0;
    }
    zval *status = zend_hash_index_find(Z_ARRVAL_P(value), 0);
    zval *headers = zend_hash_index_find(Z_ARRVAL_P(value), 1);
    zval *body = zend_hash_index_find(Z_ARRVAL_P(value), 2);
    if (!status || Z_TYPE_P(status) != IS_LONG || !headers || Z_TYPE_P(headers) != IS_ARRAY || !body || Z_TYPE_P(body) != IS_STRING) {
        return 0;
    }

    response->status = (uint16_t) Z_LVAL_P(status);
    response->headers = pack_lines(Z_ARRVAL_P(headers), &response->headers_len);
    response->body_len = Z_STRLEN_P(body);
    response->body = malloc(response->body_len ? response->body_len : 1);
    memcpy(response->body, Z_STRVAL_P(body), response->body_len);
    return 1;
}

/** tenma_respond(int $rid, array $response): void. Answers request $rid with [status, headers, body]. */
PHP_FUNCTION(tenma_respond) {
    zend_long rid;
    zval *tuple;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_LONG(rid)
        Z_PARAM_ARRAY(tuple)
    ZEND_PARSE_PARAMETERS_END();

    // Checked before the cast to 16 bits: 65736 would turn into 200
    zval *status = zend_hash_index_find(Z_ARRVAL_P(tuple), 0);
    if (status && Z_TYPE_P(status) == IS_LONG && (Z_LVAL_P(status) < 100 || Z_LVAL_P(status) > 999)) {
        zend_value_error("HTTP status " ZEND_LONG_FMT " is not between 100 and 999", Z_LVAL_P(status));
        RETURN_THROWS();
    }
    tenma_response response;
    if (!read_response(tuple, &response)) {
        zend_argument_value_error(2, "must be [int $status, list<string> $headers, string $body]");
        RETURN_THROWS();
    }
    responded_rid = (uint64_t) rid;
    tenma_rs_respond((uint64_t) rid, &response);
}

/** tenma_op_sleep(int $milliseconds): int. Starts a timer, returns the op ID. */
PHP_FUNCTION(tenma_op_sleep) {
    zend_long milliseconds;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(milliseconds)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_sleep(milliseconds < 0 ? 0 : milliseconds));
}

/** tenma_op_fetch(string $method, string $url, list<string> $headers, string $body, int $timeoutMs): int. Starts an HTTP request. */
PHP_FUNCTION(tenma_op_fetch) {
    zend_string *method, *url, *body;
    zval *headers;
    zend_long timeout_ms;
    ZEND_PARSE_PARAMETERS_START(5, 5)
        Z_PARAM_STR(method)
        Z_PARAM_STR(url)
        Z_PARAM_ARRAY(headers)
        Z_PARAM_STR(body)
        Z_PARAM_LONG(timeout_ms)
    ZEND_PARSE_PARAMETERS_END();

    size_t packed_len;
    char *packed = pack_lines(Z_ARRVAL_P(headers), &packed_len);
    uint64_t op = tenma_rs_fetch(ZSTR_VAL(method), ZSTR_LEN(method), ZSTR_VAL(url), ZSTR_LEN(url),
        packed, packed_len, ZSTR_VAL(body), ZSTR_LEN(body), timeout_ms > 0 ? (uint64_t) timeout_ms : 1);
    free(packed);
    RETURN_LONG((zend_long) op);
}

/**
 * Fields end up in PHP's internal $_POST via the filter extension (plus raw in its own array),
 * files in PHP's internal $_FILES: clears everything from the previous request, which
 * filter_input(INPUT_POST) would show otherwise.
 */
static void forget_post(void) {
    zval *post = &PG(http_globals)[TRACK_VARS_POST];
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    zval_ptr_dtor(post);
    array_init(post);
    zval_ptr_dtor(files);
    array_init(files);
    zval_ptr_dtor(&IF_G(post_array));
    ZVAL_UNDEF(&IF_G(post_array));
}

/**
 * tenma_set_input(string $body): void. Begins a classic request: php://input yields $body,
 * uploads and temp files of the previous request go away.
 */
PHP_FUNCTION(tenma_set_input) {
    zend_string *body;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STR(body)
    ZEND_PARSE_PARAMETERS_END();

    if (SG(rfc1867_uploaded_files)) {
        destroy_uploaded_files_hash();
    }
    forget_post();
    if (SG(request_info).request_body) {
        php_stream_close(SG(request_info).request_body);
    }
    php_stream *stream = php_stream_temp_create_ex(TEMP_STREAM_DEFAULT, SAPI_POST_BLOCK_SIZE, PG(upload_tmp_dir));
    php_stream_write(stream, ZSTR_VAL(body), ZSTR_LEN(body));
    php_stream_rewind(stream);
    SG(request_info).request_body = stream;
    SG(request_info).content_length = (zend_long) ZSTR_LEN(body);
}

/**
 * tenma_parse_multipart(string $contentType, string $body): array{0: array, 1: array}. Splits a
 * multipart/form-data body into [$_POST, $_FILES] with PHP's own parser; uploaded files are ready
 * as temp files, and is_uploaded_file() and move_uploaded_file() know them.
 */
PHP_FUNCTION(tenma_parse_multipart) {
    zend_string *content_type, *body;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STR(content_type)
        Z_PARAM_STR(body)
    ZEND_PARSE_PARAMETERS_END();

    forget_post();
    zval *post = &PG(http_globals)[TRACK_VARS_POST];
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    post_data = ZSTR_VAL(body);
    post_len = ZSTR_LEN(body);
    post_pos = 0;
    SG(read_post_bytes) = 0;
    SG(post_read) = 0;

    char *type = estrndup(ZSTR_VAL(content_type), ZSTR_LEN(content_type));
    rfc1867_post_handler(type, post);
    efree(type);
    post_data = NULL;

    array_init(return_value);
    Z_TRY_ADDREF_P(post);
    add_next_index_zval(return_value, post);
    Z_TRY_ADDREF_P(files);
    add_next_index_zval(return_value, files);
}

/**
 * tenma_forget_session(): void. The session extension keeps the ID until the PHP request ends,
 * and in Tenma that lasts as long as the worker. Without an ID, session_start() looks in the cookie
 * again on the next request. Only call this when the session is closed.
 */
PHP_FUNCTION(tenma_forget_session) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (PS(session_status) != php_session_active && PS(id)) {
        zend_string_release(PS(id));
        PS(id) = NULL;
    }
}

/** tenma_stream_start(int $rid, int $status, list<string> $headers): bool. Sends the head; the body follows. */
PHP_FUNCTION(tenma_stream_start) {
    zend_long rid, status;
    zval *headers;
    ZEND_PARSE_PARAMETERS_START(3, 3)
        Z_PARAM_LONG(rid)
        Z_PARAM_LONG(status)
        Z_PARAM_ARRAY(headers)
    ZEND_PARSE_PARAMETERS_END();
    if (status < 100 || status > 999) {
        zend_value_error("HTTP status " ZEND_LONG_FMT " is not between 100 and 999", status);
        RETURN_THROWS();
    }
    size_t packed_len;
    char *packed = pack_lines(Z_ARRVAL_P(headers), &packed_len);
    int started = tenma_rs_stream_start((uint64_t) rid, (uint16_t) status, packed, packed_len);
    free(packed);
    RETURN_BOOL(started);
}

/** tenma_stream_write(int $rid, string $chunk): int. 0 written, above 0 an op to wait on, -1 client gone. */
PHP_FUNCTION(tenma_stream_write) {
    zend_long rid;
    zend_string *chunk;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_LONG(rid)
        Z_PARAM_STR(chunk)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_stream_write((uint64_t) rid, ZSTR_VAL(chunk), ZSTR_LEN(chunk)));
}

PHP_FUNCTION(tenma_stream_end) {
    zend_long rid;
    bool finished;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_LONG(rid)
        Z_PARAM_BOOL(finished)
    ZEND_PARSE_PARAMETERS_END();
    tenma_rs_stream_end((uint64_t) rid, finished);
}

/** tenma_ws_accept(int $rid): bool. Responds with 101; from then on the connection is a WebSocket. */
PHP_FUNCTION(tenma_ws_accept) {
    zend_long rid;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(rid)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(tenma_rs_ws_accept((uint64_t) rid));
}

/** tenma_ws_send(int $rid, string $data): int. 0 queued, above 0 an op to wait on, -1 socket closed. */
PHP_FUNCTION(tenma_ws_send) {
    zend_long rid;
    zend_string *text;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_LONG(rid)
        Z_PARAM_STR(text)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_ws_send((uint64_t) rid, ZSTR_VAL(text), ZSTR_LEN(text)));
}

/** tenma_ws_receive(int $rid): int. Op for the next message; 0 if the socket does not exist. */
PHP_FUNCTION(tenma_ws_receive) {
    zend_long rid;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(rid)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_ws_receive((uint64_t) rid));
}

PHP_FUNCTION(tenma_ws_close) {
    zend_long rid;
    bool finished;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_LONG(rid)
        Z_PARAM_BOOL(finished)
    ZEND_PARSE_PARAMETERS_END();
    tenma_rs_ws_close((uint64_t) rid, finished);
}

/** tenma_op_db(string $op, string $target, string $sql, string $paramsJson): int. Starts a database op. */
PHP_FUNCTION(tenma_op_db) {
    zend_string *op, *target, *sql, *params;
    ZEND_PARSE_PARAMETERS_START(4, 4)
        Z_PARAM_STR(op)
        Z_PARAM_STR(target)
        Z_PARAM_STR(sql)
        Z_PARAM_STR(params)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_db(ZSTR_VAL(op), ZSTR_LEN(op), ZSTR_VAL(target), ZSTR_LEN(target),
        ZSTR_VAL(sql), ZSTR_LEN(sql), ZSTR_VAL(params), ZSTR_LEN(params)));
}

/**
 * A classic request ends as under FPM: only once no PHP code is running does Tenma run the
 * request's shutdown functions and then collect the response (finish_classic). This way PHP's own
 * rules for exceptions, exit() and fatal errors in them apply unchanged.
 *
 * tenma_classic_begin() marks the start and sets aside the shutdown functions already
 * registered (e.g. at script startup); they still run only when the worker ends.
 */

/** Records the ini values already changed, e.g. at script startup. */
static void snapshot_ini(void) {
    ALLOC_HASHTABLE(ini_snapshot);
    zend_hash_init(ini_snapshot, 8, NULL, ZVAL_PTR_DTOR, 0);
    if (!EG(modified_ini_directives)) {
        return;
    }
    zend_ini_entry *entry;
    ZEND_HASH_MAP_FOREACH_PTR(EG(modified_ini_directives), entry) {
        zval value;
        if (entry->value) {
            ZVAL_STR_COPY(&value, entry->value);
        } else {
            ZVAL_NULL(&value);
        }
        zend_hash_update(ini_snapshot, entry->name, &value);
    } ZEND_HASH_FOREACH_END();
}

/**
 * Resets what the request changed with ini_set() or set_time_limit(), as PHP does at the end of every
 * request. Resetting max_execution_time also turns off the timer.
 */
static void restore_ini(void) {
    if (!ini_snapshot) {
        return;
    }
    if (EG(modified_ini_directives)) {
        // Collect the names first: zend_restore_ini_entry deletes from the table we iterate over
        zend_string **names = safe_emalloc(zend_hash_num_elements(EG(modified_ini_directives)), sizeof(zend_string *), 0);
        uint32_t count = 0;
        zend_ini_entry *entry;
        ZEND_HASH_MAP_FOREACH_PTR(EG(modified_ini_directives), entry) {
            names[count++] = zend_string_copy(entry->name);
        } ZEND_HASH_FOREACH_END();
        for (uint32_t i = 0; i < count; i++) {
            zval *before = zend_hash_find(ini_snapshot, names[i]);
            if (!before) {
                zend_restore_ini_entry(names[i], ZEND_INI_STAGE_RUNTIME);
            } else if (Z_TYPE_P(before) == IS_STRING) {
                zend_ini_entry *now = zend_hash_find_ptr(EG(ini_directives), names[i]);
                if (now && (!now->value || !zend_string_equals(now->value, Z_STR_P(before)))) {
                    zend_alter_ini_entry_ex(names[i], Z_STR_P(before), ZEND_INI_SYSTEM, ZEND_INI_STAGE_RUNTIME, 1);
                }
            }
            zend_string_release(names[i]);
        }
        efree(names);
    }
    zend_hash_destroy(ini_snapshot);
    FREE_HASHTABLE(ini_snapshot);
    ini_snapshot = NULL;
}

/**
 * Closes the request's php://input streams. They point to the body stream without refcounting,
 * and the next request replaces it; left open, they would read freed memory.
 */
static void close_input_streams(void) {
    uint32_t capacity = zend_hash_num_elements(&EG(regular_list)), count = 0;
    if (!capacity) {
        return;
    }
    php_stream **streams = safe_emalloc(capacity, sizeof(php_stream *), 0);
    zend_resource *resource;
    ZEND_HASH_FOREACH_PTR(&EG(regular_list), resource) {
        php_stream *stream = resource->ptr;
        // NO_FCLOSE: a C owner holds the stream by pointer without a reference (SplFileObject, a libxml
        // reader) and closes it itself; closing it here would leave that owner with freed memory
        if (resource->type == php_file_le_stream() && stream && stream->ops && stream->ops->label && strcmp(stream->ops->label, "Input") == 0
            && !(stream->flags & PHP_STREAM_FLAG_NO_FCLOSE)) {
            streams[count++] = stream;
        }
    } ZEND_HASH_FOREACH_END();
    for (uint32_t i = 0; i < count; i++) {
        php_stream_close(streams[i]);
    }
    efree(streams);
}

PHP_FUNCTION(tenma_classic_begin) {
    ZEND_PARSE_PARAMETERS_NONE();
    outer_shutdown_functions = BG(user_shutdown_function_names);
    BG(user_shutdown_function_names) = NULL;
    smart_str_free(&classic_output);
    snapshot_ini();
    classic_open = 1;
}

/** tenma_take_output(): string. The classic request's output collected so far; empty afterwards. */
PHP_FUNCTION(tenma_take_output) {
    ZEND_PARSE_PARAMETERS_NONE();
    zend_string *output = smart_str_extract(&classic_output);
    RETURN_STR(output);
}

/** tenma_db_abandon(int $rid): void. Discards a request's open transactions, e.g. after exit(). */
PHP_FUNCTION(tenma_db_abandon) {
    zend_long rid;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(rid)
    ZEND_PARSE_PARAMETERS_END();
    tenma_rs_db_abandon((uint64_t) rid);
}

/**
 * tenma_parse_cookies(string $header, bool $global): array. Parses a Cookie header with PHP's own parser, exactly
 * like $_COOKIE under FPM: the first of several same-named cookies wins (protection against cookies
 * from sibling subdomains), names like a[b] become arrays, dots and spaces become underscores.
 * With $global they stay in PHP's cookie globals (classic mode, for filter_input()).
 */
PHP_FUNCTION(tenma_parse_cookies) {
    zend_string *header;
    bool global;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STR(header)
        Z_PARAM_BOOL(global)
    ZEND_PARSE_PARAMETERS_END();
    // The filter extension also collects cookies raw; without clearing, that would grow with every request
    zval_ptr_dtor(&IF_G(cookie_array));
    ZVAL_UNDEF(&IF_G(cookie_array));
    SG(request_info).cookie_data = ZSTR_VAL(header);
    sapi_module.treat_data(PARSE_COOKIE, NULL, NULL);
    SG(request_info).cookie_data = NULL;
    RETVAL_COPY(&PG(http_globals)[TRACK_VARS_COOKIE]);
    if (!global) {
        // Fiber mode: many requests share the interpreter, so one request's cookies must not
        // reach another through filter_input(INPUT_COOKIE)
        zval_ptr_dtor(&IF_G(cookie_array));
        ZVAL_UNDEF(&IF_G(cookie_array));
        zval_ptr_dtor(&PG(http_globals)[TRACK_VARS_COOKIE]);
        array_init(&PG(http_globals)[TRACK_VARS_COOKIE]);
    }
}

/** tenma_op_cancel(int $op): void. Cancels a running op, e.g. the sleep of a cancelled timer. */
PHP_FUNCTION(tenma_op_cancel) {
    zend_long op;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(op)
    ZEND_PARSE_PARAMETERS_END();
    tenma_rs_op_cancel((uint64_t) op);
}

/** tenma_exit_process(): void. exit() outside a request: tenma exits with the exit status. */
PHP_FUNCTION(tenma_exit_process) {
    ZEND_PARSE_PARAMETERS_NONE();
    // Like the PHP CLI: only the lower 8 bits count, exit(256) exits with 0
    tenma_rs_exit(EG(exit_status) & 0xff);
}

/**
 * Resolves a path like PHP's file functions do: relative to the interpreter's working directory (chdir())
 * and checked against open_basedir. NULL after a thrown exception.
 */
static char *resolve_path(zend_string *path) {
    char *resolved = expand_filepath(ZSTR_VAL(path), NULL);
    if (!resolved) {
        zend_throw_exception_ex(spl_ce_RuntimeException, 0, "Cannot resolve path: %s", ZSTR_VAL(path));
        return NULL;
    }
    if (php_check_open_basedir(resolved)) {
        zend_throw_exception_ex(spl_ce_RuntimeException, 0, "open_basedir forbids %s", resolved);
        efree(resolved);
        return NULL;
    }
    return resolved;
}

/**
 * tenma_op_fs(string $op, string $path, string $second, bool $flag, string $data): int. Starts a
 * file system op; $second is the destination for rename and copy, $flag means recursive. PHP already
 * rejects null bytes in paths at call time.
 */
PHP_FUNCTION(tenma_op_fs) {
    zend_string *op, *path, *second, *data;
    bool flag;
    ZEND_PARSE_PARAMETERS_START(5, 5)
        Z_PARAM_STR(op)
        Z_PARAM_PATH_STR(path)
        Z_PARAM_PATH_STR(second)
        Z_PARAM_BOOL(flag)
        Z_PARAM_STR(data)
    ZEND_PARSE_PARAMETERS_END();

    char *target = resolve_path(path);
    if (!target) {
        RETURN_THROWS();
    }
    char *destination = NULL;
    if (ZSTR_LEN(second) > 0 && !(destination = resolve_path(second))) {
        efree(target);
        RETURN_THROWS();
    }
    uint64_t id = tenma_rs_fs(ZSTR_VAL(op), ZSTR_LEN(op), target, strlen(target),
        destination ? destination : "", destination ? strlen(destination) : 0, flag, ZSTR_VAL(data), ZSTR_LEN(data));
    efree(target);
    if (destination) {
        efree(destination);
    }
    RETURN_LONG((zend_long) id);
}

/** tenma_pubsub(string $op, int $id, string $channel, string $message): int. Messages between workers (src/pubsub.rs). */
PHP_FUNCTION(tenma_pubsub) {
    zend_string *op, *channel, *message;
    zend_long id;
    ZEND_PARSE_PARAMETERS_START(4, 4)
        Z_PARAM_STR(op)
        Z_PARAM_LONG(id)
        Z_PARAM_STR(channel)
        Z_PARAM_STR(message)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_pubsub(ZSTR_VAL(op), ZSTR_LEN(op), (uint64_t) id, ZSTR_VAL(channel), ZSTR_LEN(channel),
        ZSTR_VAL(message), ZSTR_LEN(message)));
}

/** tenma_op_dev(string $op, string $arg): int. Starts an op for the hooks of tenma.json (src/dev.rs). */
PHP_FUNCTION(tenma_op_dev) {
    zend_string *op, *arg;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STR(op)
        Z_PARAM_STR(arg)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_dev(ZSTR_VAL(op), ZSTR_LEN(op), ZSTR_VAL(arg), ZSTR_LEN(arg)));
}

/** Forgets the last PHP error so after_bailout does not attribute it to a later request. */
static void forget_last_error(void) {
    PG(last_error_type) = 0;
    if (PG(last_error_message)) {
        zend_string_release(PG(last_error_message));
        PG(last_error_message) = NULL;
    }
}

/** How a call into PHP ended. */
enum outcome { CALL_FATAL = -1, CALL_EXCEPTION = 0, CALL_OK = 1, CALL_EXIT = 2 };

/**
 * After a zend_bailout: if it was a fatal error, the interpreter is unusable after this request.
 * Otherwise it was exit() or die(): PHP has unwound everything cleanly and only jumps out
 * because Tenma called the handler from outside.
 */
static enum outcome after_bailout(void) {
    // The longjmp skipped the frames; the pointer points into a C stack that no longer exists.
    // PHP itself resets it at this point (php_request_shutdown: "points into nirvana").
    EG(current_execute_data) = NULL;
    // A handled out-of-memory error stays set until php_request_shutdown resets it, before the
    // shutdown functions; until then every call into PHP fails, and Tenma's request never ends
    EG(delayed_oom).state = ZEND_OOM_STATE_NONE;
    if (PG(last_error_type) & E_FATAL_ERRORS) {
        return CALL_FATAL;
    }
    CG(unclean_shutdown) = 0;
    // zend_bailout switched the cycle collector off; PHP only switches it on at the next request startup
    gc_protect(0);
    return CALL_EXIT;
}

/** Calls a standard library function and reports how it ended. */
static enum outcome call_stdlib(const char *name, uint32_t count, zval *arguments) {
    zval function, retval;
    ZVAL_STRING(&function, name);
    ZVAL_UNDEF(&retval);
    volatile enum outcome result = CALL_FATAL;
    forget_last_error();
    zend_try {
        call_user_function(NULL, NULL, &function, &retval, count, arguments);
        result = EG(exception) ? CALL_EXCEPTION : CALL_OK;
        report_exception();
    } zend_catch {
        result = after_bailout();
    } zend_end_try();
    zval_ptr_dtor(&function);
    zval_ptr_dtor(&retval);
    return result;
}

/**
 * exit() in fiber mode ends only the request that was running: Tenma\Internal\exited() answers it.
 * Returns what tenma_handle and complete report.
 */
static int after_exit_in_fiber(void) {
    return call_stdlib("Tenma\\Internal\\exited", 0, NULL) == CALL_FATAL ? -1 : 1;
}

/**
 * Resumes whatever was woken up (Tenma\Internal\drain) until nothing is waiting. If one of the
 * resumed fibers ends with exit(), it carries on with the rest afterwards. -1: fatal.
 */
static int drain_ready(void) {
    for (int round = 0; round < 10000; round++) {
        enum outcome result = call_stdlib("Tenma\\Internal\\drain", 0, NULL);
        if (result == CALL_EXIT) {
            if (after_exit_in_fiber() < 0) {
                return -1;
            }
            continue;
        }
        return result == CALL_FATAL ? -1 : 0;
    }
    return 0;
}

/**
 * What tenma_handle, complete and tenma_started report after a call into the fiber world: 1, 0
 * (exception) or -1 (fatal). Afterwards everything the call woke up runs.
 */
static int fiber_result(enum outcome result) {
    int code = result == CALL_FATAL ? -1 : result == CALL_EXIT ? after_exit_in_fiber() : (int) result;
    if (code < 0) {
        return code;
    }
    return drain_ready() < 0 ? -1 : code;
}

/* ---- Native plugins (src/plugins.rs) and stop handlers ---- */

extern int tenma_rs_plugin_call(const char *name, size_t name_len, const char *function, size_t function_len,
    const char *input, size_t input_len, char **out, size_t *out_len);
extern void tenma_rs_plugin_free(char *ptr, size_t len);
extern uint64_t tenma_rs_plugin_await(const char *name, size_t name_len, const char *function, size_t function_len,
    const char *input, size_t input_len);
extern uint64_t tenma_rs_plugin_adopt(const char *name, size_t name_len, uint64_t rid);

/** tenma_plugin_call(string $plugin, string $function, string $input): string. Throws RuntimeException. */
PHP_FUNCTION(tenma_plugin_call) {
    zend_string *plugin, *function, *input;
    ZEND_PARSE_PARAMETERS_START(3, 3)
        Z_PARAM_STR(plugin)
        Z_PARAM_STR(function)
        Z_PARAM_STR(input)
    ZEND_PARSE_PARAMETERS_END();
    char *out = NULL;
    size_t out_len = 0;
    int status = tenma_rs_plugin_call(ZSTR_VAL(plugin), ZSTR_LEN(plugin), ZSTR_VAL(function), ZSTR_LEN(function),
        ZSTR_VAL(input), ZSTR_LEN(input), &out, &out_len);
    if (status != 0) {
        zend_throw_exception_ex(spl_ce_RuntimeException, 0, "%.*s", (int) out_len, out);
        tenma_rs_plugin_free(out, out_len);
        RETURN_THROWS();
    }
    RETVAL_STRINGL(out, out_len);
    tenma_rs_plugin_free(out, out_len);
}

/** tenma_op_plugin_await(string $plugin, string $function, string $input): int. The op of an asynchronous call. */
PHP_FUNCTION(tenma_op_plugin_await) {
    zend_string *plugin, *function, *input;
    ZEND_PARSE_PARAMETERS_START(3, 3)
        Z_PARAM_STR(plugin)
        Z_PARAM_STR(function)
        Z_PARAM_STR(input)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_plugin_await(ZSTR_VAL(plugin), ZSTR_LEN(plugin), ZSTR_VAL(function), ZSTR_LEN(function), ZSTR_VAL(input), ZSTR_LEN(input)));
}

/** tenma_plugin_adopt(string $plugin, int $rid): int. The connection ID, 0 if it failed. */
PHP_FUNCTION(tenma_plugin_adopt) {
    zend_string *plugin;
    zend_long rid;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STR(plugin)
        Z_PARAM_LONG(rid)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_plugin_adopt(ZSTR_VAL(plugin), ZSTR_LEN(plugin), (uint64_t) rid));
}

/* ---- Durable functions (src/durable.rs) ---- */

extern void tenma_rs_durable_context(const char *key, size_t len);
extern uint64_t tenma_rs_durable(const char *op, size_t op_len, const char *key, size_t key_len,
    const char *name, size_t name_len, double number);

/** tenma_durable_context(string $key): void. The durable function whose fiber runs now, '' for none. */
PHP_FUNCTION(tenma_durable_context) {
    zend_string *key;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STR(key)
    ZEND_PARSE_PARAMETERS_END();
    tenma_rs_durable_context(ZSTR_VAL(key), ZSTR_LEN(key));
}

/** tenma_op_durable(string $op, string $key, string $name, float $number): int. An op of the durable machinery. */
PHP_FUNCTION(tenma_op_durable) {
    zend_string *op, *key, *name;
    double number;
    ZEND_PARSE_PARAMETERS_START(4, 4)
        Z_PARAM_STR(op)
        Z_PARAM_STR(key)
        Z_PARAM_STR(name)
        Z_PARAM_DOUBLE(number)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG((zend_long) tenma_rs_durable(ZSTR_VAL(op), ZSTR_LEN(op), ZSTR_VAL(key), ZSTR_LEN(key), ZSTR_VAL(name), ZSTR_LEN(name), number));
}

/** Stop handlers of Process\Shutdown still running in this worker, counted by the PHP side. */
static _Thread_local int stop_handlers = 0;

/** tenma_stop_handlers(int $delta): int. A stop handler starts (1) or ends (-1); returns how many run. */
PHP_FUNCTION(tenma_stop_handlers) {
    zend_long delta;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(delta)
    ZEND_PARSE_PARAMETERS_END();
    stop_handlers += (int) delta;
    RETURN_LONG(stop_handlers);
}

int tenma_stop_handlers_running(void) {
    return stop_handlers;
}

/** tenma stops: Tenma\Internal\stopping() starts the handlers of Process\Shutdown::register(). -1: fatal. */
int tenma_stopping(void) {
    return fiber_result(call_stdlib("Tenma\\Internal\\stopping", 0, NULL)) < 0 ? -1 : 0;
}

/** The poller resumes a durable function: Threading\Durable::resume($name, $key). -1: fatal. */
int tenma_resume_durable(const char *name, size_t name_len, const char *key, size_t key_len) {
    zval arguments[2];
    ZVAL_STRINGL(&arguments[0], name, name_len);
    ZVAL_STRINGL(&arguments[1], key, key_len);
    int result = fiber_result(call_stdlib("Threading\\Durable::resume", 2, arguments));
    zval_ptr_dtor(&arguments[0]);
    zval_ptr_dtor(&arguments[1]);
    return result < 0 ? -1 : 0;
}

/** The event loop is running: Tenma\Internal\started() starts timers and tasks from script startup. -1: fatal. */
int tenma_started(void) {
    return fiber_result(call_stdlib("Tenma\\Internal\\started", 0, NULL)) < 0 ? -1 : 0;
}


/**
 * Finishes a classic request at the top level: the request's shutdown functions, then the
 * response via Tenma\Http\finishClassic($fatal). 1: answered. -2: answered, but the
 * interpreter is unusable after a fatal error. 0: not answered, Rust sends 500.
 * -1: not answered and the interpreter is unusable.
 */
static int finish_classic(uint64_t rid, enum outcome handled) {
    int fatal = handled == CALL_FATAL;
    // Do not forget the last error: shutdown functions read it with error_get_last()
    // Has its own zend_try: exit() in there only ends the shutdown functions, as in PHP
    php_call_shutdown_functions();
    EG(current_execute_data) = NULL;
    php_free_shutdown_functions();
    if (PG(last_error_type) & E_FATAL_ERRORS) {
        fatal = 1;
    }
    BG(user_shutdown_function_names) = outer_shutdown_functions;
    outer_shutdown_functions = NULL;
    if (!fatal) {
        CG(unclean_shutdown) = 0;
        // exit() in a shutdown function bailed out, which switched the cycle collector off
        gc_protect(0);
    }
    // Whatever is still in output buffers belongs to the response, including buffers that cannot be removed.
    // Their handlers are user code: a fatal error in there must not bail out without an address
    zend_try {
        php_output_end_all();
    } zend_catch {
        if (after_bailout() == CALL_FATAL) {
            fatal = 1;
        }
    } zend_end_try();

    zval argument;
    ZVAL_BOOL(&argument, fatal);
    responded_rid = 0;
    enum outcome finished = call_stdlib("Http\\finishClassic", 1, &argument);
    int answered = responded_rid == rid;
    classic_open = 0;
    smart_str_free(&classic_output);
    // User code runs here too: stream filters when php://input closes, an error handler for the warning
    // when memory_limit can't go back below what is in use. A bailout needs an address to return to
    zend_try {
        close_input_streams();
        restore_ini();
    } zend_catch {
        if (after_bailout() == CALL_FATAL) {
            fatal = 1;
        }
    } zend_end_try();
    if (finished == CALL_FATAL) {
        return -1;
    }
    // If finishClassic itself failed, e.g. while saving the session, the client gets 500 instead of waiting
    if (!answered) {
        return fatal ? -1 : 0;
    }
    return fatal ? -2 : 1;
}

/**
 * Reports a finished op to PHP: calls Tenma\Internal\complete($op, $ok, $value), which resumes the
 * waiting fiber. Returns as tenma_handle does; takes ownership of $value.
 */
static int complete(uint64_t op, zend_bool ok, zval *value) {
    zval arguments[3];
    ZVAL_LONG(&arguments[0], (zend_long) op);
    ZVAL_BOOL(&arguments[1], ok);
    ZVAL_COPY_VALUE(&arguments[2], value);
    enum outcome result = call_stdlib("Tenma\\Internal\\complete", 3, arguments);
    zval_ptr_dtor(&arguments[2]);
    return fiber_result(result);
}

/** The client of request rid left before it got a response: cancel its tasks. */
int tenma_request_gone(uint64_t rid) {
    zval argument;
    ZVAL_LONG(&argument, (zend_long) rid);
    return fiber_result(call_stdlib("Tenma\\Internal\\requestGone", 1, &argument));
}

int tenma_complete_null(uint64_t op) {
    zval value;
    ZVAL_NULL(&value);
    return complete(op, 1, &value);
}

/**
 * Does a result of len bytes no longer fit under memory_limit? Then it becomes an error in the request,
 * instead of a fatal error when creating the string, which would restart the whole worker.
 */
static int too_big(size_t len) {
    return PG(memory_limit) > 0 && zend_memory_usage(1) + len + (1 << 20) > (size_t) PG(memory_limit);
}

/** The bytes a result may still have so that it fits under memory_limit; SIZE_MAX without a limit. */
size_t tenma_memory_room(void) {
    if (PG(memory_limit) <= 0) {
        return SIZE_MAX;
    }
    size_t used = zend_memory_usage(1) + (1 << 20);
    return used < (size_t) PG(memory_limit) ? (size_t) PG(memory_limit) - used : 0;
}

int tenma_complete_error(uint64_t op, const char *message, size_t len, int code) {
    // An error arrives as [message, code]; Internal\await throws a RuntimeException with both
    zval value;
    array_init(&value);
    add_next_index_stringl(&value, message, len);
    add_next_index_long(&value, code);
    return complete(op, 0, &value);
}

static int complete_too_big(uint64_t op, size_t len) {
    char message[160];
    int written = snprintf(message, sizeof(message), "Result of %zu bytes does not fit under memory_limit", len);
    return tenma_complete_error(op, message, (size_t) written, 0);
}

int tenma_complete_string(uint64_t op, const char *text, size_t len) {
    if (too_big(len)) {
        return complete_too_big(op, len);
    }
    zval value;
    ZVAL_STRINGL(&value, text, len);
    return complete(op, 1, &value);
}

/** Response of a fetch as [int $status, list<string> $headers, string $body]; header lines as s\0s\0 … */
int tenma_complete_http(uint64_t op, uint16_t status, const char *headers, size_t headers_len, const char *body, size_t body_len) {
    if (too_big(headers_len + body_len)) {
        return complete_too_big(op, headers_len + body_len);
    }
    zval value, lines;
    array_init(&value);
    add_next_index_long(&value, status);
    array_init(&lines);
    for (size_t start = 0, i = 0; i < headers_len; i++) {
        if (headers[i] == 0) {
            add_next_index_stringl(&lines, headers + start, i - start);
            start = i + 1;
        }
    }
    add_next_index_zval(&value, &lines);
    add_next_index_stringl(&value, body, body_len);
    return complete(op, 1, &value);
}

/**
 * Calls a server's handler with a request and its ID. The response comes via
 * tenma_respond, immediately or later from a fiber. 1: handler completed. 0: exception, Rust
 * responds with 500. -1: fatal error, PHP is no longer usable. -2: answered, but afterwards
 * PHP is no longer usable (fatal error in a classic request).
 */
int tenma_handle(uint32_t handler, uint64_t rid, const tenma_request *request) {
    if (handler >= handler_count) {
        return 0;
    }
    // Allocating beyond memory_limit outside zend_try would end the whole process
    if (too_big(request->body.len + request->uri.len)) {
        return 0;
    }
    volatile enum outcome result = CALL_FATAL;
    zval arguments[2], headers, retval;
    zval *argument = &arguments[0];
    ZVAL_LONG(&arguments[1], (zend_long) rid);
    array_init(argument);
    add_str(argument, "method", request->method);
    add_str(argument, "uri", request->uri);
    add_str(argument, "body", request->body);
    add_str(argument, "remote_addr", request->remote_addr);
    add_assoc_long(argument, "remote_port", request->remote_port);
    add_str(argument, "server_addr", request->server_addr);
    add_assoc_long(argument, "server_port", request->server_port);
    add_assoc_bool(argument, "https", request->https);
    add_str(argument, "protocol", request->protocol);
    array_init(&headers);
    for (size_t i = 0; i < request->header_count; i++) {
        const tenma_header *header = &request->headers[i];
        // A repeated header joins its values (names come lowercase), as proxies and PHP-FPM do
        zval *existing = zend_hash_str_find(Z_ARRVAL(headers), header->name.ptr, header->name.len);
        if (existing) {
            int cookie = header->name.len == 6 && memcmp(header->name.ptr, "cookie", 6) == 0;
            zend_string *joined = zend_string_concat3(Z_STRVAL_P(existing), Z_STRLEN_P(existing), cookie ? "; " : ", ", 2, header->value.ptr, header->value.len);
            zval_ptr_dtor(existing);
            ZVAL_STR(existing, joined);
        } else {
            add_assoc_stringl_ex(&headers, header->name.ptr, header->name.len, header->value.ptr, header->value.len);
        }
    }
    add_assoc_zval(argument, "headers", &headers);
    ZVAL_UNDEF(&retval);
    reset_headers();

    forget_last_error();
    zend_try {
        call_user_function(NULL, NULL, &handlers[handler], &retval, 2, arguments);
        result = EG(exception) ? CALL_EXCEPTION : CALL_OK;
        report_exception();
    } zend_catch {
        result = after_bailout();
    } zend_end_try();

    zval_ptr_dtor(argument);
    zval_ptr_dtor(&retval);
    if (classic_open) {
        return finish_classic(rid, result);
    }
    return fiber_result(result);
}

void tenma_response_free(tenma_response *response) {
    free(response->body);
    free(response->headers);
}
