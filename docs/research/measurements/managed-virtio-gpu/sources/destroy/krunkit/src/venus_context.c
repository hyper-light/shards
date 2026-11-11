#include "venus_context.h"

#include "util/hash_table.h"

#include "virglrenderer.h"
#include "virgl_context.h"
#include "virgl_util.h"

#include "venus/vkr_renderer.h"

struct venus_fence {
   uint32_t flags;
   uint32_t seqno;
   uint64_t fence_id;
   struct list_head head;
};

struct list_head contexts;

static inline void
venus_context_resource_add(struct venus_context *ctx, uint32_t res_id)
{
   assert(!_mesa_hash_table_search(ctx->resource_table, (void *)(uintptr_t)res_id));
   _mesa_hash_table_insert(ctx->resource_table, (void *)(uintptr_t)res_id, NULL);
}

static inline bool
venus_context_resource_find(struct venus_context *ctx, uint32_t res_id)
{
   return _mesa_hash_table_search(ctx->resource_table, (void *)(uintptr_t)res_id);
}

static inline void
venus_context_resource_remove(struct venus_context *ctx, uint32_t res_id)
{
   _mesa_hash_table_remove_key(ctx->resource_table, (void *)(uintptr_t)res_id);
}

static inline bool
venus_context_resource_table_init(struct venus_context *ctx)
{
   ctx->resource_table = _mesa_hash_table_create_u32_keys(NULL);
   return ctx->resource_table;
}

static inline void
venus_context_resource_table_fini(struct venus_context *ctx)
{
   _mesa_hash_table_destroy(ctx->resource_table, NULL);
}

static void
venus_context_attach_resource(struct virgl_context *base, struct virgl_resource *res)
{
   struct venus_context *ctx = (struct venus_context *)base;
   const uint32_t res_id = res->res_id;

   /* avoid importing resources created from RENDER_CONTEXT_OP_CREATE_RESOURCE */
   if (venus_context_resource_find(ctx, res_id))
      return;

   /* The current render protocol only supports importing dma-buf or pipe resource that
    * can be exported to dma-buf. A protocol change is needed when there exists use case
    * for importing external Vulkan opaque resource. For shm, we only create with blob_id
    * 0 via CREATE_RESOURCE above.
    */
   assert(res->fd_type == VIRGL_RESOURCE_FD_INVALID ||
          res->fd_type == VIRGL_RESOURCE_FD_DMABUF);

   enum virgl_resource_fd_type res_fd_type = res->fd_type;
   uint64_t res_size = res->map_size;

   if (!vkr_renderer_import_resource(base->ctx_id, res_id, res_fd_type, -1, res_size)) {
      //fprintf(stderr, "failed to import res %d", res_id);
   }

   venus_context_resource_add(ctx, res_id);
}

static void
venus_context_detach_resource(struct virgl_context *base, struct virgl_resource *res)
{
   struct venus_context *ctx = (struct venus_context *)base;
   const uint32_t res_id = res->res_id;

   /* avoid detaching resource not belonging to this context */
   if (!venus_context_resource_find(ctx, res_id))
      return;

   vkr_renderer_destroy_resource(base->ctx_id, res_id);

   venus_context_resource_remove(ctx, res_id);
}

static int
venus_context_transfer_3d(struct virgl_context *base,
                          struct virgl_resource *res,
                          UNUSED const struct vrend_transfer_info *info,
                          UNUSED int transfer_mode)
{
   struct venus_context *ctx = (struct venus_context *)base;

   //fprintf(stderr, "no transfer support for ctx %d and res %d", ctx->base.ctx_id, res->res_id);
   return -1;
}

static int
venus_context_get_blob(struct virgl_context *base,
                       uint32_t res_id,
                       uint64_t blob_id,
                       uint64_t blob_size,
                       uint32_t blob_flags,
                       struct virgl_context_blob *blob)
{
   /* RENDER_CONTEXT_OP_CREATE_RESOURCE implies resource attach, thus proxy tracks
    * resources created here to avoid double attaching the same resource when proxy is on
    * attach_resource callback.
    */
   struct venus_context *ctx = (struct venus_context *)base;
   int res_fd;
   enum virgl_resource_fd_type fd_type;
   uint32_t map_info;
   uint64_t map_ptr;
   struct virgl_resource_vulkan_info vulkan_info;

   if (!vkr_renderer_create_resource(base->ctx_id, res_id, blob_id, blob_size, blob_flags,
                                     &fd_type, &res_fd, &map_info, &map_ptr, &vulkan_info)) {
      //fprintf(stderr, "can't create res %d with blob %llu\n", res_id, blob_id);
      return -1;
   }

   //fprintf(stderr, "%s: map_ptr=0x%llx\n", __func__, map_ptr);

   blob->type = fd_type;
   blob->u.fd = res_fd;
   blob->map_info = map_info;
   blob->map_ptr = map_ptr;
   blob->vulkan_info = vulkan_info;

   //fprintf(stderr, "%s: fd=0x%llx\n", __func__, map_ptr);

   venus_context_resource_add(ctx, res_id);

   return 0;
}

static int
venus_context_submit_cmd(struct virgl_context *base, const void *buffer, size_t size)
{
   struct venus_context *ctx = (struct venus_context *)base;
   void *cmd = (void *)buffer;

   if (!size)
      return 0;

   if (!vkr_renderer_submit_cmd(base->ctx_id, cmd, size)) {
      //fprintf(stderr, "error processing command");
      return -1;
   }

   return 0;
}

static bool
venus_fence_is_signaled(const struct venus_fence *fence, uint32_t cur_seqno)
{
   /* takes wrapping into account */
   const uint32_t d = cur_seqno - fence->seqno;
   return d < INT32_MAX;
}

static struct venus_fence *
venus_context_alloc_fence(struct venus_context *ctx)
{
   struct venus_fence *fence = NULL;

   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_lock(&ctx->free_fences_mutex);

   if (!list_is_empty(&ctx->free_fences)) {
      fence = list_first_entry(&ctx->free_fences, struct venus_fence, head);
      list_del(&fence->head);
   }

   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_unlock(&ctx->free_fences_mutex);

   return fence ? fence : malloc(sizeof(*fence));
}

static void
venus_context_free_fence(struct venus_context *ctx, struct venus_fence *fence)
{
   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_lock(&ctx->free_fences_mutex);

   list_add(&fence->head, &ctx->free_fences);

   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_unlock(&ctx->free_fences_mutex);
}

#if 0
static uint32_t
venus_context_load_timeline_seqno(struct venus_context *ctx, uint32_t ring_idx)
{
   return atomic_load(&ctx->timeline_seqnos[ring_idx]);
}
#endif

static void
venus_context_retire_fences_internal(struct venus_context *ctx, uint32_t ring_idx, uint32_t seqno)
{
   struct venus_timeline *timeline = &ctx->timelines[ring_idx];

   timeline->cur_seqno = seqno;
   //timeline->cur_seqno_stall_count = 0;

   //fprintf(stderr, "%s: ring_idx=%d seqno=%d\n", __func__, ring_idx, seqno);

   list_for_each_entry_safe (struct venus_fence, fence, &timeline->fences, head) {
      if (!venus_fence_is_signaled(fence, timeline->cur_seqno))
         return;

      //fprintf(stderr, "%s: fence_retire ring_idx=%d seqno=%d\n", __func__, ring_idx, seqno);
      ctx->base.fence_retire(&ctx->base, ring_idx, fence->fence_id);

      list_del(&fence->head);
      venus_context_free_fence(ctx, fence);
   }
}

static void
venus_context_retire_fences(struct virgl_context *base)
{
   fprintf(stderr, "%s: UNIMPLEMENTED\n", __func__);
#if 0
   struct proxy_context *ctx = (struct proxy_context *)base;

   uint64_t new_busy_mask = 0;
   uint64_t old_busy_mask = ctx->timeline_busy_mask;
   while (old_busy_mask) {
      const uint32_t ring_idx = u_bit_scan64(&old_busy_mask);
      const uint32_t cur_seqno = proxy_context_load_timeline_seqno(ctx, ring_idx);
      if (!proxy_context_retire_timeline_fences_locked(ctx, ring_idx, cur_seqno))
         new_busy_mask |= 1ull << ring_idx;
   }

   ctx->timeline_busy_mask = new_busy_mask;

   if (proxy_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
      mtx_unlock(&ctx->timeline_mutex);
#endif
}

static int
venus_context_get_fencing_fd(struct virgl_context *base)
{
   struct venus_context *ctx = (struct venus_context *)base;

   //fprintf(stderr, "%s: UNIMPLEMENTED\n", __func__);

   //assert(!(venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB));
   //return ctx->sync_thread.fence_eventfd;
}

static int
venus_context_submit_fence(struct virgl_context *base,
                           uint32_t flags,
                           uint32_t ring_idx,
                           uint64_t fence_id)
{
   struct venus_context *ctx = (struct venus_context *)base;
   const uint64_t old_busy_mask = ctx->timeline_busy_mask;

   //fprintf(stderr, "%s: ring_idx=%d fence_id=%llu\n", __func__, ring_idx, fence_id);

   if (ring_idx >= VENUS_CONTEXT_TIMELINE_COUNT)
      return -EINVAL;

   struct venus_timeline *timeline = &ctx->timelines[ring_idx];
   struct venus_fence *fence = venus_context_alloc_fence(ctx);
   if (!fence)
      return -ENOMEM;

   fence->flags = flags;
   fence->seqno = timeline->next_seqno++;
   fence->fence_id = fence_id;

   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_lock(&ctx->timeline_mutex);

   list_addtail(&fence->head, &timeline->fences);
   ctx->timeline_busy_mask |= 1ull << ring_idx;

   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_unlock(&ctx->timeline_mutex);

   if (vkr_renderer_submit_fence(base->ctx_id, flags, ring_idx, fence_id))
      return 0;

   /* recover timeline fences and busy_mask on submit_fence request failure */
   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_lock(&ctx->timeline_mutex);

   list_del(&fence->head);
   ctx->timeline_busy_mask = old_busy_mask;

   //if (venus_renderer.flags & VIRGL_RENDERER_ASYNC_FENCE_CB)
   //   mtx_unlock(&ctx->timeline_mutex);

   venus_context_free_fence(ctx, fence);
   //fprintf(stderr, "%s: failed to submit fence\n", __func__);
   return -1;
}


static struct venus_context *
venus_lookup_context(uint32_t ctx_id)
{
   struct venus_context *ctx = NULL;

   list_for_each_entry (struct venus_context, iter, &contexts, head) {
      if (iter->base.ctx_id == ctx_id) {
         ctx = iter;
         break;
      }
   }

   return ctx;
}

static void
venus_add_context(struct venus_context *ctx)
{
   list_addtail(&ctx->head, &contexts);
}

static void
venus_remove_context(struct venus_context *ctx)
{
   list_del(&ctx->head);
}

static void
venus_context_destroy(struct virgl_context *base)
{
   struct venus_context *ctx = (struct venus_context *)base;

   vkr_renderer_destroy_context(base->ctx_id);

   list_for_each_entry_safe (struct venus_fence, fence, &ctx->free_fences, head)
      free(fence);
   venus_context_resource_table_fini(ctx);

   venus_remove_context(ctx);

   free(ctx);
}

static void
venus_context_init_base(struct venus_context *ctx)
{
   ctx->base.destroy = venus_context_destroy;
   ctx->base.attach_resource = venus_context_attach_resource;
   ctx->base.detach_resource = venus_context_detach_resource;
   ctx->base.transfer_3d = venus_context_transfer_3d;
   ctx->base.get_blob = venus_context_get_blob;
   ctx->base.submit_cmd = venus_context_submit_cmd;

   ctx->base.get_fencing_fd = venus_context_get_fencing_fd;
   ctx->base.retire_fences = venus_context_retire_fences;
   ctx->base.submit_fence = venus_context_submit_fence;
}

static void
venus_context_init_timelines(struct venus_context *ctx)
{
   for (uint32_t i = 0; i < ARRAY_SIZE(ctx->timelines); i++) {
      struct venus_timeline *timeline = &ctx->timelines[i];
      list_inithead(&timeline->fences);
   }
}

struct virgl_context *
venus_context_create(uint32_t ctx_id,
                     uint32_t ctx_flags,
                     size_t debug_len,
                     const char *debug_name)
{
   struct venus_context *ctx;

   //fprintf(stderr, "%s: entry\n", __func__);

   if (!vkr_renderer_create_context(ctx_id, ctx_flags, debug_len, debug_name)) {
      return NULL;
   }

   ctx = calloc(1, sizeof(*ctx));
   if (!ctx) {
      return NULL;
   }

   venus_context_init_base(ctx);
   venus_context_resource_table_init(ctx);
   venus_context_init_timelines(ctx);
   list_inithead(&ctx->free_fences);
   venus_add_context(ctx);

   return &ctx->base;
}

static void
venus_state_cb_debug_logger(UNUSED enum virgl_log_level_flags log_level,
                             const char *message,
                             UNUSED void* user_data)
{
   //fprintf(stderr, "%s: %s\n", __func__, message);
}

static void
venus_state_cb_retire_fence(uint32_t ctx_id, uint32_t ring_idx, uint64_t fence_id)
{
   struct venus_context *ctx = venus_lookup_context(ctx_id);
   assert(ctx);

   const uint32_t seqno = (uint32_t)fence_id;
   venus_context_retire_fences_internal(ctx, ring_idx, seqno);
}

static const struct vkr_renderer_callbacks venus_state_cbs = {
   .debug_logger = venus_state_cb_debug_logger,
   .retire_fence = venus_state_cb_retire_fence,
};

int
venus_renderer_init(void)
{
   //fprintf(stderr, "%s: entry\n", __func__);

   list_inithead(&contexts);

   static const uint32_t required_flags =
      VKR_RENDERER_THREAD_SYNC | VKR_RENDERER_ASYNC_FENCE_CB;

   if (!vkr_renderer_init(required_flags, &venus_state_cbs)) {
      return -1;
   }

   return 0;
}

void
venus_renderer_reset(void)
{
   // TODO
}

size_t
venus_get_capset(uint32_t set, void *caps)
{
   switch (set) {
   case VIRGL_RENDERER_CAPSET_VENUS:
      return vkr_get_capset(caps);
   default:
      break;
   }

   return 0;
}
