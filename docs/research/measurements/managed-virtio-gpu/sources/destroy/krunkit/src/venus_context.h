#ifndef VENUS_CONTEXT_H_
#define VENUS_CONTEXT_H_

#include <assert.h>
#include <errno.h>
#include <inttypes.h>
#include <stdatomic.h>
#include <stdbool.h>

#include "util/hash_table.h"
#include "util/list.h"
#include "util/macros.h"

#include "virgl_context.h"

#define VENUS_CONTEXT_TIMELINE_COUNT 64

static_assert(ATOMIC_INT_LOCK_FREE == 2, "proxy renderer requires lock-free atomic_uint");

struct venus_timeline {
   uint32_t cur_seqno;
   uint32_t next_seqno;
   struct list_head fences;

   int cur_seqno_stall_count;
};

struct venus_context {
   struct virgl_context base;

   struct list_head head;

   /* this tracks resources early attached in get_blob */
   struct hash_table *resource_table;

   //mtx_t timeline_mutex;
   struct venus_timeline timelines[VENUS_CONTEXT_TIMELINE_COUNT];
   /* which timelines have fences */
   uint64_t timeline_busy_mask;
   /* this points a region of shmem updated by the render worker */
   const volatile atomic_uint *timeline_seqnos;

   //mtx_t free_fences_mutex;
   struct list_head free_fences;
};

int
venus_renderer_init(void);

void
venus_renderer_reset(void);

size_t
venus_get_capset(uint32_t set, void *caps);

struct virgl_context *
venus_context_create(uint32_t ctx_id,
                     uint32_t ctx_flags,
                     size_t debug_len,
                     const char *debug_name);

#endif // VENUS_CONTEXT_H_
