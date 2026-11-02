// Passive PipeWire control listener. Its loop fd is driven by Tokio, never a
// PipeWire thread or an audio/data loop. No audio buffers or write APIs are used.
#include <pipewire/pipewire.h>
#include <pipewire/impl-module.h>
#include <pipewire/extensions/metadata.h>
#include <spa/param/param.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>

enum { CHANGED = 1, RESET = 2, DISCONNECTED = 4 };
struct events;
struct object {
    struct spa_list link;
    struct events *owner;
    struct pw_proxy *proxy;
    struct spa_hook listener;
    uint32_t id, device;
    char *name; // Only speaker nodes have a name; devices have NULL.
};
struct events {
    struct pw_loop *loop;
    struct pw_context *context;
    struct pw_core *core;
    struct pw_registry *registry;
    struct spa_hook core_listener, registry_listener;
    struct spa_list objects;
    char *default_name;
    uint32_t sink, device;
    int pending;
};

static void select_sink(struct events *self) {
    struct object *object;
    uint32_t sink = SPA_ID_INVALID, device = SPA_ID_INVALID;
    spa_list_for_each(object, &self->objects, link) {
        if (object->name && self->default_name &&
                strcmp(object->name, self->default_name) == 0) {
            sink = object->id;
            device = object->device;
            break;
        }
    }
    if (sink != self->sink || device != self->device) {
        self->sink = sink;
        self->device = device;
        self->pending |= RESET | CHANGED;
    }
}

static int metadata_property(void *data, uint32_t subject, const char *key,
        const char *type, const char *value) {
    struct object *object = data;
    struct events *self = object->owner;
    if (subject != PW_ID_CORE || (key && strcmp(key, "default.audio.sink") != 0))
        return 0;
    // Only metadata changes parse JSON, inside PipeWire's own small parser.
    struct pw_properties *props = value && type && strcmp(type, "Spa:String:JSON") == 0
        ? pw_properties_new_string(value) : NULL;
    const char *name = props ? pw_properties_get(props, "name") : NULL;
    bool changed = name ? !self->default_name || strcmp(name, self->default_name) != 0
                        : self->default_name != NULL;
    if (changed) {
        free(self->default_name);
        self->default_name = name ? strdup(name) : NULL;
        self->pending |= RESET | CHANGED;
        select_sink(self);
    }
    if (props) pw_properties_free(props);
    return 0;
}
static const struct pw_metadata_events metadata_events = {
    PW_VERSION_METADATA_EVENTS, .property = metadata_property,
};

static void node_param(void *data, int seq, uint32_t id, uint32_t index,
        uint32_t next, const struct spa_pod *param) {
    struct object *object = data;
    (void)seq; (void)index; (void)next; (void)param;
    if (id == SPA_PARAM_Props && object->id == object->owner->sink)
        object->owner->pending |= CHANGED;
}
static const struct pw_node_events node_events = {
    PW_VERSION_NODE_EVENTS, .param = node_param,
};
static void device_param(void *data, int seq, uint32_t id, uint32_t index,
        uint32_t next, const struct spa_pod *param) {
    struct object *object = data;
    (void)seq; (void)index; (void)next; (void)param;
    if (id == SPA_PARAM_Route && object->id == object->owner->device)
        object->owner->pending |= CHANGED;
}
static const struct pw_device_events device_events = {
    PW_VERSION_DEVICE_EVENTS, .param = device_param,
};

static void global(void *data, uint32_t id, uint32_t permissions,
        const char *type, uint32_t version, const struct spa_dict *props) {
    struct events *self = data;
    const char *name = props ? spa_dict_lookup(props, PW_KEY_NODE_NAME) : NULL;
    const char *media = props ? spa_dict_lookup(props, PW_KEY_MEDIA_CLASS) : NULL;
    const char *metadata = props ? spa_dict_lookup(props, PW_KEY_METADATA_NAME) : NULL;
    bool node = strcmp(type, PW_TYPE_INTERFACE_Node) == 0 && media &&
        strcmp(media, "Audio/Sink") == 0 && name;
    bool device = strcmp(type, PW_TYPE_INTERFACE_Device) == 0;
    bool defaults = strcmp(type, PW_TYPE_INTERFACE_Metadata) == 0 && metadata &&
        strcmp(metadata, "default") == 0;
    (void)permissions;
    if (!node && !device && !defaults) return;
    struct object *object = calloc(1, sizeof(*object));
    if (!object) { self->pending |= DISCONNECTED; return; }
    object->owner = self;
    object->id = id;
    object->device = SPA_ID_INVALID;
    object->proxy = pw_registry_bind(self->registry, id, type,
        SPA_MIN(version, (uint32_t)(node ? PW_VERSION_NODE : device ? PW_VERSION_DEVICE : PW_VERSION_METADATA)), 0);
    if (!object->proxy) { free(object); self->pending |= DISCONNECTED; return; }
    spa_list_append(&self->objects, &object->link);
    if (node) {
        const char *device_id = spa_dict_lookup(props, PW_KEY_DEVICE_ID);
        object->name = strdup(name);
        if (device_id) object->device = (uint32_t)strtoul(device_id, NULL, 10);
        pw_node_add_listener((struct pw_node *)object->proxy, &object->listener, &node_events, object);
        uint32_t params[] = { SPA_PARAM_Props };
        pw_node_subscribe_params((struct pw_node *)object->proxy, params, 1);
        select_sink(self);
    } else if (device) {
        pw_device_add_listener((struct pw_device *)object->proxy, &object->listener, &device_events, object);
        uint32_t params[] = { SPA_PARAM_Route };
        pw_device_subscribe_params((struct pw_device *)object->proxy, params, 1);
    } else {
        pw_metadata_add_listener((struct pw_metadata *)object->proxy, &object->listener, &metadata_events, object);
    }
}
static void remove_global(void *data, uint32_t id) {
    struct events *self = data;
    struct object *object, *next;
    spa_list_for_each_safe(object, next, &self->objects, link) {
        if (object->id != id) continue;
        if (id == self->device) self->pending |= RESET | CHANGED;
        // Metadata removal loses the default-speaker identity too.
        if (!object->name && object->id != self->device) {
            const char *type = pw_proxy_get_type(object->proxy, NULL);
            if (strcmp(type, PW_TYPE_INTERFACE_Metadata) == 0) {
                free(self->default_name); self->default_name = NULL;
                self->pending |= RESET | CHANGED;
            }
        }
        spa_hook_remove(&object->listener);
        pw_proxy_destroy(object->proxy);
        spa_list_remove(&object->link);
        free(object->name); free(object);
        select_sink(self);
    }
}
static const struct pw_registry_events registry_events = {
    PW_VERSION_REGISTRY_EVENTS, .global = global, .global_remove = remove_global,
};
static void core_error(void *data, uint32_t id, int seq, int res, const char *message) {
    struct events *self = data;
    (void)seq; (void)res; (void)message;
    if (id == PW_ID_CORE) self->pending |= DISCONNECTED;
}
static const struct pw_core_events core_events = {
    PW_VERSION_CORE_EVENTS, .error = core_error,
};

void pc2mqtt_events_free(struct events *self) {
    if (!self) return;
    while (!spa_list_is_empty(&self->objects)) {
        struct object *object = spa_list_first(&self->objects, struct object, link);
        remove_global(self, object->id);
    }
    if (self->registry) pw_proxy_destroy((struct pw_proxy *)self->registry);
    if (self->core) pw_core_disconnect(self->core);
    if (self->context) pw_context_destroy(self->context);
    if (self->loop) { pw_loop_leave(self->loop); pw_loop_destroy(self->loop); }
    free(self->default_name); free(self);
}
struct events *pc2mqtt_events_new(const char *socket) {
    pw_init(NULL, NULL);
    struct events *self = calloc(1, sizeof(*self));
    if (!self) return NULL;
    spa_list_init(&self->objects);
    self->sink = self->device = SPA_ID_INVALID;
    self->loop = pw_loop_new(NULL);
    if (!self->loop) goto error;
    pw_loop_enter(self->loop);
    self->context = pw_context_new(self->loop, pw_properties_new(
        PW_KEY_CONFIG_NAME, "null", "context.num-data-loops", "0", NULL), 0);
    if (!self->context) goto error;
    // Load only the wire protocol and metadata marshaling. In particular, never
    // load a user's client configuration, realtime module, or stream factories.
    if (!pw_context_load_module(self->context, "libpipewire-module-protocol-native", NULL, NULL) ||
            !pw_context_load_module(self->context, "libpipewire-module-metadata", NULL, NULL)) goto error;
    self->core = pw_context_connect(self->context,
        pw_properties_new(PW_KEY_REMOTE_NAME, socket, PW_KEY_APP_NAME, "pc2mqtt audio events", NULL), 0);
    if (!self->core) goto error;
    pw_core_add_listener(self->core, &self->core_listener, &core_events, self);
    self->registry = pw_core_get_registry(self->core, PW_VERSION_REGISTRY, 0);
    if (!self->registry) goto error;
    pw_registry_add_listener(self->registry, &self->registry_listener, &registry_events, self);
    return self;
error:
    pc2mqtt_events_free(self);
    return NULL;
}
int pc2mqtt_events_fd(struct events *self) { return pw_loop_get_fd(self->loop); }
int pc2mqtt_events_dispatch(struct events *self) {
    if (pw_loop_iterate(self->loop, 0) < 0) self->pending |= DISCONNECTED;
    int pending = self->pending;
    self->pending = 0;
    return pending;
}
