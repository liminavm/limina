# GTK4 aborts when `vkGetPipelineCacheData` fails: unchecked result + uninitialised size

*Draft for GNOME/gtk. Not filed yet — see "Before filing" at the end.*

## Summary

`gdk_vulkan_save_pipeline_cache()` queries the pipeline-cache size with `vkGetPipelineCacheData`
and **discards the result**, then `g_malloc()`s whatever is in the uninitialised `size` local. When
the driver fails that call — which the specification explicitly permits — `size` is never written,
so the application aborts in `g_malloc()` on a garbage length.

This is a crash of any GTK4 application using the Vulkan renderer, triggered from a timer, with no
user action involved.

## Impact

Observed as `gnome-control-center` dying with `SIGABRT` while merely open. The abort comes from
`gdk_vulkan_save_pipeline_cache_cb`, i.e. the periodic cache-save timeout, so any window that has
been open long enough is exposed; the user's action at the time is irrelevant.

```
GLib: ../glib/gmem.c:106: failed to allocate 281474368445232 bytes
```

`281474368445232` is `0xFFFF_0000_0FF0`-ish — stack residue, not a plausible cache size.

Backtrace:

```
#0  __pthread_kill_implementation ()      libc.so.6
#1  raise ()                              libc.so.6
#2  abort ()                              libc.so.6
#3  _g_log_abort.lto_priv.0 ()            libglib-2.0.so.0
#4  g_log_default_handler ()              libglib-2.0.so.0
#5  g_logv ()                             libglib-2.0.so.0
#6  g_log ()                              libglib-2.0.so.0
#7  g_malloc ()                           libglib-2.0.so.0
#8  gdk_vulkan_save_pipeline_cache.isra () libgtk-4.so.1
#9  gdk_vulkan_save_pipeline_cache_cb ()  libgtk-4.so.1
#10 g_timeout_dispatch ()                 libglib-2.0.so.0
#11 g_main_context_dispatch_unlocked ()   libglib-2.0.so.0
...
#14 g_application_run ()                  libgio-2.0.so.0
#15 main ()
```

Immediately before the abort, GTK itself logged the failure it then ignored:

```
vkGetPipelineCacheData(): A host memory allocation has failed. (VK_ERROR_OUT_OF_HOST_MEMORY) (-1)
```

## Analysis

`gdk/gdkvulkancontext.c`, `gdk_vulkan_save_pipeline_cache()`:

```c
static gboolean
gdk_vulkan_save_pipeline_cache (GdkDisplay *display)
{
  ...
  size_t size;                     /* (1) uninitialised */
  char *data, *etag;

  device = display->vk_device;
  cache = display->vk_pipeline_cache;

  GDK_VK_CHECK (vkGetPipelineCacheData, device, cache, &size, NULL);   /* (2) result discarded */
  if (size == 0)
    return TRUE;

  if (size == display->vk_pipeline_cache_size)
    {
      GDK_DEBUG (VULKAN, "pipeline cache size (%zu bytes) unchanged, skipping save", size);
      return TRUE;
    }

  data = g_malloc (size);                                              /* (3) aborts */
  if (GDK_VK_CHECK (vkGetPipelineCacheData, device, cache, &size, data) != VK_SUCCESS)
    {
      g_free (data);
      return FALSE;
    }
```

Three separate things have to line up, and they do:

1. **`size` is uninitialised.** A Vulkan command that returns an error is not required to write its
   output parameters, so on failure `size` keeps stack residue.
2. **The size query's result is dropped, while the data query's is checked.** The second
   `GDK_VK_CHECK` is used exactly right — `!= VK_SUCCESS` → bail. The first one is called purely
   for its side effect. That asymmetry is the bug; note that `GDK_VK_CHECK` *does* return the
   result, so the value was available at the call site:

   ```c
   static inline VkResult
   gdk_vulkan_handle_result (VkResult res, const char *called_function)
   {
     if (res != VK_SUCCESS)
       g_warning ("%s(): %s (%d)", called_function, gdk_vulkan_strerror (res), res);
     return res;
   }
   ```

   This is why the warning appears in the log a moment before the abort: GTK detected the failure,
   reported it, and then used the uninitialised size regardless.
3. **`g_malloc()` aborts rather than returning NULL** (GLib's documented contract), so an
   implausible length is immediately fatal to the process rather than a failed allocation the
   caller could handle.

`vkGetPipelineCacheData` is documented to return `VK_ERROR_OUT_OF_HOST_MEMORY` or
`VK_ERROR_OUT_OF_DEVICE_MEMORY`, so a driver returning one of those is conformant behaviour that
GTK must survive. Any driver under real memory pressure at the moment the save timer fires will
reproduce this.

## Suggested fix

```diff
-  size_t size;
+  size_t size = 0;
   char *data, *etag;
 
-  GDK_VK_CHECK (vkGetPipelineCacheData, device, cache, &size, NULL);
+  if (GDK_VK_CHECK (vkGetPipelineCacheData, device, cache, &size, NULL) != VK_SUCCESS)
+    return FALSE;
   if (size == 0)
     return TRUE;
 
   if (size == display->vk_pipeline_cache_size)
     {
       GDK_DEBUG (VULKAN, "pipeline cache size (%zu bytes) unchanged, skipping save", size);
       return TRUE;
     }
 
-  data = g_malloc (size);
+  data = g_try_malloc (size);
+  if (data == NULL)
+    {
+      g_warning_once ("Failed to allocate %zu bytes for the pipeline cache", size);
+      return FALSE;
+    }
```

Checking the result is the actual fix. Initialising `size` is defence in depth — it alone would
turn the crash into the existing `size == 0` early return. `g_try_malloc` is worth having because
the length comes from outside GTK: a driver bug (or a corrupted cache) should not be able to abort
the application through an allocation it merely reports.

## How it was hit

A bug in our own Vulkan stack (a virglrenderer/venus host renderer) poisoned the guest's renderer
context, after which every host-memory allocation the guest driver attempted failed. That made
`vkGetPipelineCacheData` return `VK_ERROR_OUT_OF_HOST_MEMORY` reliably, which is how a
normally-rare driver failure became a reproducible GTK crash. Our side is fixed independently —
this report is only about GTK's handling of a failure the specification allows.

Environment: aarch64 Fedora 44 guest, GNOME on Wayland, Mesa venus driver on a virtio-gpu.

## Before filing

- [ ] Pin the GTK version. The code above is from `main`; the crash was on Fedora 44's `gtk4`
      package. Confirm the shipped version has the same shape (the backtrace frames and the
      warning string match, so it almost certainly does) and quote that version in the report.
- [ ] Check for an existing issue — searching `gdk_vulkan_save_pipeline_cache` and
      `vkGetPipelineCacheData` in GNOME/gtk issues.
- [ ] Optionally offer the patch as an MR rather than only an issue; it is small and mechanical.
