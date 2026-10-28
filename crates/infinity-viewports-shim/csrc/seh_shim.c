/*
   we'll keep the shim minimal and as safe as possible here. Rust was giving me
   trouble catching Win32's structured exceptions so we can just wrap the call
   and report errors back to rust.
*/

#include <string.h>
#include <windows.h>

int infinity_viewports_guarded_read(void *dst, const void *src, size_t len) {
  _try {
    memcpy(dst, src, len);
    return 1;
  }
  __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_read_ptr(const void *src, void **out) {
  __try {
    *out = *(void *const *)src;
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_call2(void (*fn)(void *, void *), void *rcx,
                                     void *rdx) {
  __try {
    fn(rcx, rdx);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_call2_ret(unsigned long long (*fn)(void *,
                                                                  void *),
                                         void *rcx, void *rdx,
                                         unsigned long long *out) {
  __try {
    *out = fn(rcx, rdx);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_write(void *dst, const void *src, size_t len) {
  __try {
    memcpy(dst, src, len);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_call1(void (*fn)(void *), void *rcx) {
  __try {
    fn(rcx);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_call1_ret(void *(*fn)(void *), void *rcx,
                                         void **out) {
  __try {
    *out = fn(rcx);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_activate(void (*fn)(void *, unsigned long long,
                                                   int),
                                        void *mgr, unsigned long long id,
                                        int open) {
  __try {
    fn(mgr, id, open);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_set_fov(void (*fn)(void *, float), void *camera,
                                       float fov) {
  __try {
    fn(camera, fov);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_attach(void (*fn)(void *, const void *, int),
                                      void *parent, const void *handle,
                                      int keep_world) {
  __try {
    fn(parent, handle, keep_world);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}

int infinity_viewports_guarded_detach(void (*fn)(void *, int, int), void *node,
                                      int keep_children, int keep_world) {
  __try {
    fn(node, keep_children, keep_world);
    return 1;
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    return 0;
  }
}
