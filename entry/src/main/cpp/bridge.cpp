#include <ace/xcomponent/native_interface_xcomponent.h>
#include <native_window/external_window.h>
#include <native_vsync/native_vsync.h>
#include <napi/native_api.h>
#include <hilog/log.h>
#include <arkui/ui_input_event.h>
#include <atomic>
#include <algorithm>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <string>
#include <vector>

// Rust worker entry points.
extern "C" {
bool lc_init(const uint8_t*, size_t, void (*)(uint64_t, const uint8_t*, size_t, const uint8_t*, size_t));
void lc_attach(uintptr_t, uint32_t, uint32_t);
void lc_resize(uint32_t, uint32_t);
void lc_detach();
void lc_frame();
void lc_active(bool);
void lc_close();
void lc_input(const uint8_t*, size_t);
void lc_reply(uint64_t, bool, const uint8_t*, size_t, const uint8_t*, size_t);
void lc_request_vsync();
}

namespace {
std::mutex notifyMutex;
napi_threadsafe_function notifyFn = nullptr;
OH_NativeVSync* vsync = nullptr;
std::atomic<bool> framePending(false);
std::atomic<bool> attached(false);

struct WinState {
    std::atomic<int32_t> touchId{-1};
    std::atomic<bool> penContact{false};
    std::atomic<bool> fingerContact{false};
    std::set<int32_t> palmIds;
};
std::mutex winMutex;
std::map<OH_NativeXComponent*, std::shared_ptr<WinState>> windows;
thread_local WinState* cur = nullptr;
WinState* Enter(OH_NativeXComponent* c) {
    std::lock_guard<std::mutex> lock(winMutex);
    auto it = windows.find(c);
    cur = it == windows.end() ? nullptr : it->second.get();
    return cur;
}
struct Request { uint64_t id; std::string json; std::vector<uint8_t> data; };
void CallJS(napi_env env, napi_value callback, void*, void* ptr) {
    auto* r = static_cast<Request*>(ptr);
    if (env && callback) {
        napi_value args[3], result, receiver;
        napi_create_double(env, static_cast<double>(r->id), &args[0]);
        napi_create_string_utf8(env, r->json.data(), r->json.size(), &args[1]);
        void* data = nullptr;
        napi_create_arraybuffer(env, r->data.size(), &data, &args[2]);
        if (!r->data.empty()) std::memcpy(data, r->data.data(), r->data.size());
        napi_get_undefined(env, &receiver);
        if (napi_call_function(env, receiver, callback, 3, args, &result) != napi_ok && r->id) {
            const std::string error = "platform callback failed";
            lc_reply(r->id, false, reinterpret_cast<const uint8_t*>(error.data()), error.size(), nullptr, 0);
        }
    } else if (r->id) {
        const std::string error = "cancelled";
        lc_reply(r->id, false, reinterpret_cast<const uint8_t*>(error.data()), error.size(), nullptr, 0);
    }
    delete r;
}
void NotifyRust(uint64_t id, const uint8_t* json, size_t n, const uint8_t* data, size_t size) {
    auto* r = new Request{id, std::string(reinterpret_cast<const char*>(json), n), {}};
    if (size) r->data.assign(data, data + size);
    std::lock_guard<std::mutex> lock(notifyMutex);
    if (!notifyFn || napi_call_threadsafe_function(notifyFn, r, napi_tsfn_nonblocking) != napi_ok) {
        delete r;
        if (id) { const std::string e = "cancelled"; lc_reply(id, false, reinterpret_cast<const uint8_t*>(e.data()), e.size(), nullptr, 0); }
    }
}
void Vsync(long long, void*) { framePending = false; if (attached) lc_frame(); }
std::string String(napi_env env, napi_value v) {
    size_t len = 0;
    if (!v || napi_get_value_string_utf8(env, v, nullptr, 0, &len) != napi_ok || len > 1024*1024) return {};
    std::vector<char> data(len + 1);
    if (napi_get_value_string_utf8(env, v, data.data(), data.size(), &len) != napi_ok) return {};
    return std::string(data.data(), len);
}
napi_value Undefined(napi_env env) { napi_value v; napi_get_undefined(env, &v); return v; }
void Json(const std::string& s) { lc_input(reinterpret_cast<const uint8_t*>(s.data()), s.size()); }

void SurfaceCreated(OH_NativeXComponent* c, void* window) {
    auto* state = Enter(c);
    if (!state) return;
    uint64_t w = 0, h = 0;
    if (!window || OH_NativeXComponent_GetXComponentSize(c, window, &w, &h) != OH_NATIVEXCOMPONENT_RESULT_SUCCESS) return;
    // Take one owned reference the worker will release when it drops the surface; without it
    // the handle can outlive the callback and the unreference in Rust would be unbalanced.
    if (OH_NativeWindow_NativeObjectReference(window) != 0) return;
    attached = true;
    lc_attach(reinterpret_cast<uintptr_t>(window), static_cast<uint32_t>(w), static_cast<uint32_t>(h));
}
void SurfaceChanged(OH_NativeXComponent* c, void* window) {
    auto* state = Enter(c);
    if (!state) return;
    uint64_t w = 0, h = 0;
    if (OH_NativeXComponent_GetXComponentSize(c, window, &w, &h) != OH_NATIVEXCOMPONENT_RESULT_SUCCESS) return;
    lc_resize(static_cast<uint32_t>(w), static_cast<uint32_t>(h));
}
void SurfaceDestroyed(OH_NativeXComponent* c, void*) {
    auto* state = Enter(c);
    if (!state) return;
    state->touchId = -1; state->penContact = false; state->fingerContact = false; state->palmIds.clear();
    attached = false; lc_detach();
}
void Touch(OH_NativeXComponent* c, void* window) {
    constexpr double palmAreaThreshold = 0.65;
    auto* state = Enter(c);
    if (!state) return;
    OH_NativeXComponent_TouchEvent e{};
    if (OH_NativeXComponent_GetTouchEvent(c, window, &e) != OH_NATIVEXCOMPONENT_RESULT_SUCCESS) return;
    uint32_t index = 0;
    while (index < e.numPoints && index < OH_NATIVE_XCOMPONENT_MAX_TOUCH_POINTS_NUMBER && e.touchPoints[index].id != e.id) ++index;
    OH_NativeXComponent_TouchPointToolType tool = OH_NATIVEXCOMPONENT_TOOL_TYPE_FINGER;
    if (index < e.numPoints && index < OH_NATIVE_XCOMPONENT_MAX_TOUCH_POINTS_NUMBER)
        OH_NativeXComponent_GetTouchPointToolType(c, index, &tool);
    bool pen = tool == OH_NATIVEXCOMPONENT_TOOL_TYPE_PEN || tool == OH_NATIVEXCOMPONENT_TOOL_TYPE_RUBBER;
    if (!pen) {
        if (e.type == OH_NATIVEXCOMPONENT_CANCEL) {
            state->fingerContact = false; state->palmIds.clear();
            Json("{\"kind\":\"blur\"}");
            return;
        }
        std::string points;
        std::set<int32_t> active;
        for (uint32_t i = 0; i < e.numPoints && i < OH_NATIVE_XCOMPONENT_MAX_TOUCH_POINTS_NUMBER; ++i) {
            const auto& p = e.touchPoints[i];
            OH_NativeXComponent_TouchPointToolType pt = OH_NATIVEXCOMPONENT_TOOL_TYPE_FINGER;
            OH_NativeXComponent_GetTouchPointToolType(c, i, &pt);
            if (pt != OH_NATIVEXCOMPONENT_TOOL_TYPE_FINGER || p.type == OH_NATIVEXCOMPONENT_UP ||
                p.type == OH_NATIVEXCOMPONENT_CANCEL || (p.id == e.id && e.type == OH_NATIVEXCOMPONENT_UP) ||
                !std::isfinite(p.x) || !std::isfinite(p.y)) continue;
            active.insert(p.id);
            if (p.id == e.id && e.type == OH_NATIVEXCOMPONENT_DOWN &&
                std::isfinite(p.size) && p.size >= palmAreaThreshold && p.size <= 1.0) state->palmIds.insert(p.id);
            if (state->palmIds.count(p.id)) continue;
            if (!points.empty()) points += ",";
            points += "{\"id\":" + std::to_string(static_cast<uint32_t>(p.id)) + ",\"x\":" + std::to_string(p.x) + ",\"y\":" + std::to_string(p.y) + "}";
        }
        for (auto it = state->palmIds.begin(); it != state->palmIds.end();) {
            if (!active.count(*it)) it = state->palmIds.erase(it); else ++it;
        }
        state->fingerContact = !active.empty();
        Json("{\"kind\":\"touch\",\"points\":[" + points + "]}");
        return;
    }
    int action = 2;
    if (e.type == OH_NATIVEXCOMPONENT_DOWN) { if (state->touchId != -1) Json("{\"kind\":\"pointer\",\"action\":3}"); state->touchId = e.id; state->penContact = true; action = 0; }
    else if (e.type == OH_NATIVEXCOMPONENT_UP) action = 1;
    else if (e.type == OH_NATIVEXCOMPONENT_CANCEL) action = 3;
    if (state->touchId != e.id) return;
    Json("{\"kind\":\"pointer\",\"source\":\"pen\",\"action\":" + std::to_string(action) + ",\"x\":" + std::to_string(e.x) + ",\"y\":" + std::to_string(e.y) + "}");
    if (action == 1 || action == 3) state->touchId = -1;
}
void Mouse(OH_NativeXComponent* c, void* window) {
    auto* state = Enter(c);
    if (!state) return;
    if (state->penContact || state->fingerContact) return;
    OH_NativeXComponent_MouseEvent e{};
    if (OH_NativeXComponent_GetMouseEvent(c, window, &e) != OH_NATIVEXCOMPONENT_RESULT_SUCCESS) return;
    if (e.action == OH_NATIVEXCOMPONENT_MOUSE_CANCEL) { Json("{\"kind\":\"pointer\",\"action\":3}"); return; }
    if (e.action != OH_NATIVEXCOMPONENT_MOUSE_PRESS && e.action != OH_NATIVEXCOMPONENT_MOUSE_RELEASE && e.action != OH_NATIVEXCOMPONENT_MOUSE_MOVE) return;
    int action = e.action == OH_NATIVEXCOMPONENT_MOUSE_PRESS ? 0 : e.action == OH_NATIVEXCOMPONENT_MOUSE_RELEASE ? 1 : 2;
    int button = e.button == OH_NATIVEXCOMPONENT_RIGHT_BUTTON ? 1 : e.button == OH_NATIVEXCOMPONENT_MIDDLE_BUTTON ? 2 : 0;
    Json("{\"kind\":\"pointer\",\"source\":\"mouse\",\"action\":" + std::to_string(action) + ",\"button\":" + std::to_string(button) + ",\"x\":" + std::to_string(e.x) + ",\"y\":" + std::to_string(e.y) + "}");
}
bool Key(OH_NativeXComponent* c, void*) {
    if (!Enter(c)) return false;
    OH_NativeXComponent_KeyEvent* event = nullptr;
    OH_NativeXComponent_KeyAction action = OH_NATIVEXCOMPONENT_KEY_ACTION_UNKNOWN;
    OH_NativeXComponent_KeyCode code = static_cast<OH_NativeXComponent_KeyCode>(-1);
    if (OH_NativeXComponent_GetKeyEvent(c, &event) != 0 || !event ||
        OH_NativeXComponent_GetKeyEventAction(event, &action) != 0 ||
        OH_NativeXComponent_GetKeyEventCode(event, &code) != 0) return false;
    uint64_t modifiers = 0; bool caps = false;
    OH_NativeXComponent_GetKeyEventModifierKeyStates(event, &modifiers);
    OH_NativeXComponent_GetKeyEventCapsLockState(event, &caps);
    bool ctrl = modifiers & 1, shift = modifiers & 2, alt = modifiers & 4;
    int key = static_cast<int>(code);
    std::string text;
    if (!ctrl && !alt && action == OH_NATIVEXCOMPONENT_KEY_ACTION_DOWN) {
        if (key >= 2017 && key <= 2042) text.push_back(static_cast<char>((shift != caps ? 'A' : 'a') + key - 2017));
        else if (key >= 2000 && key <= 2009) text.push_back(shift ? ")!@#$%^&*("[key - 2000] : static_cast<char>('0' + key - 2000));
        else if (key == 2050) text = " ";
    }
    Json("{\"kind\":\"key\",\"action\":" + std::to_string(static_cast<int>(action)) +
         ",\"code\":" + std::to_string(key) +
         ",\"ctrl\":" + (ctrl ? "true" : "false") +
         ",\"shift\":" + (shift ? "true" : "false") +
         ",\"alt\":" + (alt ? "true" : "false") +
         ",\"text\":\"" + text + "\"}");
    return true;
}
void Axis(OH_NativeXComponent* c, ArkUI_UIInputEvent* event, ArkUI_UIInputEvent_Type type) {
    auto* state = Enter(c);
    if (!state) return;
    if (!event || type != ARKUI_UIINPUTEVENT_TYPE_AXIS) return;
    double x = -OH_ArkUI_AxisEvent_GetHorizontalAxisValue(event);
    double y = -OH_ArkUI_AxisEvent_GetVerticalAxisValue(event);
    double zoom = OH_ArkUI_AxisEvent_GetPinchAxisScaleValue(event);
    if (!std::isfinite(zoom) || zoom < 0) zoom = 0;
    Json("{\"kind\":\"axis\",\"dx\":" + std::to_string(x) + ",\"dy\":" + std::to_string(y) + ",\"zoom\":" + std::to_string(zoom) + "}");
}
void Blur(OH_NativeXComponent* c, void*) {
    auto* state = Enter(c);
    if (!state) return;
    state->touchId = -1; state->penContact = false; state->fingerContact = false; state->palmIds.clear();
    Json("{\"kind\":\"blur\"}");
}
OH_NativeXComponent_Callback callbacks{SurfaceCreated, SurfaceChanged, SurfaceDestroyed, Touch};
OH_NativeXComponent_MouseEvent_Callback mouseCallbacks{Mouse, nullptr};

bool RegisterComponent(OH_NativeXComponent* c) {
    if (!c) return false;
    {
        std::lock_guard<std::mutex> lock(winMutex);
        auto it = windows.find(c);
        if (it == windows.end()) windows[c] = std::make_shared<WinState>();
    }
    OH_NativeXComponent_RegisterCallback(c, &callbacks);
    OH_NativeXComponent_RegisterMouseEventCallback(c, &mouseCallbacks);
    OH_NativeXComponent_RegisterKeyEventCallbackWithResult(c, Key);
    OH_NativeXComponent_RegisterBlurEventCallback(c, Blur);
    OH_NativeXComponent_RegisterUIInputEventCallback(c, Axis, ARKUI_UIINPUTEVENT_TYPE_AXIS);
    return true;
}
napi_value InitHost(napi_env env, napi_callback_info info) {
    size_t n = 2; napi_value args[2]; napi_get_cb_info(env, info, &n, args, nullptr, nullptr);
    if (n != 2) return Undefined(env);
    auto config = String(env, args[0]);
    {
        std::lock_guard<std::mutex> lock(notifyMutex);
        if (notifyFn) napi_release_threadsafe_function(notifyFn, napi_tsfn_abort);
        napi_value name; napi_create_string_utf8(env, "LightCraft platform", NAPI_AUTO_LENGTH, &name);
        auto status = napi_create_threadsafe_function(env, args[1], nullptr, name, 0, 1, nullptr, nullptr, nullptr, CallJS, &notifyFn);
        if (status != napi_ok) return Undefined(env);
        napi_unref_threadsafe_function(env, notifyFn);
    }
    if (!vsync) vsync = OH_NativeVSync_Create("LightCraft", 10);
    bool started = lc_init(reinterpret_cast<const uint8_t*>(config.data()), config.size(), NotifyRust);
    if (!started) NotifyRust(0, reinterpret_cast<const uint8_t*>("{\"kind\":\"error\",\"text\":\"Native init failed\"}"), 49, nullptr, 0);
    return Undefined(env);
}
napi_value Input(napi_env env, napi_callback_info info) {
    size_t n = 1; napi_value arg; napi_get_cb_info(env, info, &n, &arg, nullptr, nullptr);
    if (n) { auto s = String(env, arg); Json(s); }
    return Undefined(env);
}
napi_value Reply(napi_env env, napi_callback_info info) {
    size_t n = 4; napi_value args[4]; napi_get_cb_info(env, info, &n, args, nullptr, nullptr);
    if (n != 4) return Undefined(env);
    double id = 0; bool ok = false; void* data = nullptr; size_t size = 0;
    napi_get_value_double(env, args[0], &id); napi_get_value_bool(env, args[1], &ok);
    auto text = String(env, args[2]); napi_get_arraybuffer_info(env, args[3], &data, &size);
    lc_reply(static_cast<uint64_t>(id), ok, reinterpret_cast<const uint8_t*>(text.data()), text.size(), static_cast<uint8_t*>(data), size);
    return Undefined(env);
}
napi_value Active(napi_env env, napi_callback_info info) {
    size_t n = 1; napi_value arg; bool active = false; napi_get_cb_info(env, info, &n, &arg, nullptr, nullptr);
    if (n) napi_get_value_bool(env, arg, &active); lc_active(active);
    return Undefined(env);
}
napi_value Close(napi_env env, napi_callback_info) { lc_close(); return Undefined(env); }
void Cleanup(void*) {
    attached = false; lc_active(false); lc_detach();
    std::lock_guard<std::mutex> lock(notifyMutex);
    if (notifyFn) { napi_release_threadsafe_function(notifyFn, napi_tsfn_abort); notifyFn = nullptr; }
}
napi_value Init(napi_env env, napi_value exports) {
    const napi_property_descriptor props[] = {
        {"init", nullptr, InitHost, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"input", nullptr, Input, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"reply", nullptr, Reply, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"active", nullptr, Active, nullptr, nullptr, nullptr, napi_default, nullptr},
        {"close", nullptr, Close, nullptr, nullptr, nullptr, napi_default, nullptr},
    };
    napi_define_properties(env, exports, sizeof(props)/sizeof(props[0]), props);
    napi_value value; OH_NativeXComponent* c = nullptr;
    if (napi_get_named_property(env, exports, OH_NATIVE_XCOMPONENT_OBJ, &value) == napi_ok &&
        napi_unwrap(env, value, reinterpret_cast<void**>(&c)) == napi_ok && c) {
        RegisterComponent(c);
    }
    napi_add_env_cleanup_hook(env, Cleanup, nullptr);
    return exports;
}
napi_module module{1, 0, nullptr, Init, "lightcraft", nullptr, {0}};
}
extern "C" void lc_request_vsync() {
    if (vsync && attached && !framePending.exchange(true)) {
        if (OH_NativeVSync_RequestFrame(vsync, Vsync, nullptr) != 0) framePending = false;
    }
}
extern "C" __attribute__((constructor)) void RegisterLightCraft() { napi_module_register(&module); }
