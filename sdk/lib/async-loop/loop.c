// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef _ALL_SOURCE
#define _ALL_SOURCE  // Enables thrd_create_with_name in <threads.h>.
#endif
#include <assert.h>
#include <lib/async-loop/loop.h>
#include <lib/async/irq.h>
#include <lib/async/paged_vmo.h>
#include <lib/async/receiver.h>
#include <lib/async/task.h>
#include <lib/async/trap.h>
#include <lib/async/wait.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <zircon/assert.h>
#include <zircon/listnode.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/hypervisor.h>

// The port wait key associated with the dispatcher's control messages.
#define KEY_CONTROL (0u)

static zx_time_t async_loop_now(async_dispatcher_t* dispatcher);
static zx_status_t async_loop_begin_wait(async_dispatcher_t* dispatcher, async_wait_t* wait);
static zx_status_t async_loop_cancel_wait(async_dispatcher_t* dispatcher, async_wait_t* wait);
static zx_status_t async_loop_post_task(async_dispatcher_t* dispatcher, async_task_t* task);
static zx_status_t async_loop_cancel_task(async_dispatcher_t* dispatcher, async_task_t* task);
static zx_status_t async_loop_queue_packet(async_dispatcher_t* dispatcher,
                                           async_receiver_t* receiver,
                                           const zx_packet_user_t* data);
static zx_status_t async_loop_set_guest_bell_trap(async_dispatcher_t* dispatcher,
                                                  async_guest_bell_trap_t* trap, zx_handle_t guest,
                                                  zx_vaddr_t addr, size_t length);
static zx_status_t async_loop_bind_irq(async_dispatcher_t* dispatcher, async_irq_t* irq);
static zx_status_t async_loop_unbind_irq(async_dispatcher_t* dispatcher, async_irq_t* irq);
static zx_status_t async_loop_create_paged_vmo(async_dispatcher_t* async,
                                               async_paged_vmo_t* paged_vmo, uint32_t options,
                                               zx_handle_t pager, uint64_t vmo_size,
                                               zx_handle_t* vmo_out);
static zx_status_t async_loop_detach_paged_vmo(async_dispatcher_t* dispatcher,
                                               async_paged_vmo_t* paged_vmo);

static const async_ops_t async_loop_ops = {
    .version = ASYNC_OPS_V2,
    .v1 =
        {
            .now = async_loop_now,
            .begin_wait = async_loop_begin_wait,
            .cancel_wait = async_loop_cancel_wait,
            .post_task = async_loop_post_task,
            .cancel_task = async_loop_cancel_task,
            .queue_packet = async_loop_queue_packet,
            .set_guest_bell_trap = async_loop_set_guest_bell_trap,
        },
    .v2 =
        {
            .bind_irq = async_loop_bind_irq,
            .unbind_irq = async_loop_unbind_irq,
            .create_paged_vmo = async_loop_create_paged_vmo,
            .detach_paged_vmo = async_loop_detach_paged_vmo,
        },
};

typedef struct thread_record {
  list_node_t node;
  thrd_t thread;
} thread_record_t;

const async_loop_config_t kAsyncLoopConfigNeverAttachToThread = {
    .make_default_for_current_thread = false,
    .default_accessors = {.getter = NULL, .setter = NULL}};

typedef struct async_loop {
  async_dispatcher_t dispatcher;  // must be first (the loop inherits from async_dispatcher_t)
  async_loop_config_t config;     // immutable
  zx_handle_t port;               // immutable
  zx_handle_t timer;              // immutable

  _Atomic async_loop_state_t state;
  atomic_uint active_threads;  // number of active dispatch threads
  atomic_uint worker_threads;  // number of worker threads created with `async_loop_start_thread`

  mtx_t lock;                  // guards the lists and the dispatching tasks flag
  bool dispatching_tasks;      // true while the loop is busy dispatching tasks
  list_node_t wait_list;       // most recently added first
  list_node_t task_list;       // pending tasks, earliest deadline first
  list_node_t due_list;        // due tasks, earliest deadline first
  list_node_t thread_list;     // earliest created thread first
  list_node_t irq_list;        // list of IRQs
  list_node_t paged_vmo_list;  // most recently added first
  bool timer_armed;            // true if timer has been set and has not fired yet
} async_loop_t;

static zx_status_t async_loop_run_once(async_loop_t* loop, zx_time_t deadline);
static zx_status_t async_loop_dispatch_wait(async_loop_t* loop, async_wait_t* wait,
                                            zx_status_t status, const zx_packet_signal_t* signal);
static zx_status_t async_loop_dispatch_irq(async_loop_t* loop, async_irq_t* irq, zx_status_t status,
                                           const zx_packet_interrupt_t* interrupt);
static zx_status_t async_loop_dispatch_tasks(async_loop_t* loop);
static void async_loop_dispatch_task(async_loop_t* loop, async_task_t* task, zx_status_t status);
static zx_status_t async_loop_dispatch_packet(async_loop_t* loop, async_receiver_t* receiver,
                                              zx_status_t status, const zx_packet_user_t* data);
static zx_status_t async_loop_dispatch_guest_bell_trap(async_loop_t* loop,
                                                       async_guest_bell_trap_t* trap,
                                                       zx_status_t status,
                                                       const zx_packet_guest_bell_t* bell);
static zx_status_t async_loop_dispatch_paged_vmo(async_loop_t* loop, async_paged_vmo_t* paged_vmo,
                                                 zx_status_t status,
                                                 const zx_packet_page_request_t* page_request);
static zx_status_t async_loop_cancel_paged_vmo(async_paged_vmo_t* paged_vmo);
static void async_loop_wake_threads(async_loop_t* loop);
static void async_loop_insert_task_locked(async_loop_t* loop, async_task_t* task);
static void async_loop_restart_timer_locked(async_loop_t* loop);
static void async_loop_invoke_prologue(async_loop_t* loop);
static void async_loop_invoke_epilogue(async_loop_t* loop);

static_assert(sizeof(list_node_t) <= sizeof(async_state_t), "async_state_t too small");

#define TO_NODE(type, ptr) ((list_node_t*)&ptr->state)
#define FROM_NODE(type, ptr) ((type*)((char*)(ptr) - offsetof(type, state)))

static inline list_node_t* wait_to_node(async_wait_t* wait) { return TO_NODE(async_wait_t, wait); }

static inline async_wait_t* node_to_wait(list_node_t* node) {
  return FROM_NODE(async_wait_t, node);
}

static inline list_node_t* irq_to_node(async_irq_t* irq) { return TO_NODE(async_irq_t, irq); }

static inline list_node_t* task_to_node(async_task_t* task) { return TO_NODE(async_task_t, task); }

static inline async_task_t* node_to_task(list_node_t* node) {
  return FROM_NODE(async_task_t, node);
}

static inline async_irq_t* node_to_irq(list_node_t* node) { return FROM_NODE(async_irq_t, node); }

static inline list_node_t* paged_vmo_to_node(async_paged_vmo_t* paged_vmo) {
  return TO_NODE(async_paged_vmo_t, paged_vmo);
}

static inline async_paged_vmo_t* node_to_paged_vmo(list_node_t* node) {
  return FROM_NODE(async_paged_vmo_t, node);
}

zx_status_t async_loop_create(const async_loop_config_t* config, async_loop_t** out_loop) {
  ZX_DEBUG_ASSERT(out_loop);
  ZX_DEBUG_ASSERT(config != NULL);
  // If a setter was given, a getter should have been, too.
  ZX_ASSERT((config->default_accessors.setter != NULL) ==
            (config->default_accessors.getter != NULL));

  async_loop_t* loop = calloc(1u, sizeof(async_loop_t));
  if (!loop)
    return ZX_ERR_NO_MEMORY;
  atomic_init(&loop->state, ASYNC_LOOP_RUNNABLE);
  atomic_init(&loop->active_threads, 0u);
  atomic_init(&loop->worker_threads, 0u);

  loop->dispatcher.ops = (const async_ops_t*)&async_loop_ops;
  loop->config = *config;
  mtx_init(&loop->lock, mtx_plain);
  list_initialize(&loop->wait_list);
  list_initialize(&loop->irq_list);
  list_initialize(&loop->task_list);
  list_initialize(&loop->due_list);
  list_initialize(&loop->thread_list);
  list_initialize(&loop->paged_vmo_list);

  zx_status_t status =
      zx_port_create(config->irq_support ? ZX_PORT_BIND_TO_INTERRUPT : 0, &loop->port);
  if (status == ZX_OK)
    status = zx_timer_create(ZX_TIMER_SLACK_LATE, ZX_CLOCK_MONOTONIC, &loop->timer);
  if (status == ZX_OK) {
    *out_loop = loop;
    if (loop->config.make_default_for_current_thread) {
      ZX_DEBUG_ASSERT(loop->config.default_accessors.getter() == NULL);
      loop->config.default_accessors.setter(&loop->dispatcher);
    }
  } else {
    // Adjust this flag so we don't trip an assert trying to clear a default dispatcher we never
    // installed.
    loop->config.make_default_for_current_thread = false;
    async_loop_destroy(loop);
  }
  return status;
}

void async_loop_destroy(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);

  async_loop_shutdown(loop);

  ZX_DEBUG_ASSERT(list_is_empty(&loop->wait_list));
  ZX_DEBUG_ASSERT(list_is_empty(&loop->irq_list));
  ZX_DEBUG_ASSERT(list_is_empty(&loop->task_list));
  ZX_DEBUG_ASSERT(list_is_empty(&loop->due_list));
  ZX_DEBUG_ASSERT(list_is_empty(&loop->thread_list));
  ZX_DEBUG_ASSERT(list_is_empty(&loop->paged_vmo_list));

  zx_handle_close(loop->port);
  zx_handle_close(loop->timer);
  mtx_destroy(&loop->lock);
  free(loop);
}

// Cancel all pending tasks with the status code ZX_ERR_CANCELED.
//
// Used during dispatcher shutdown.
static void async_loop_cancel_all(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN);

  list_node_t* node;

  mtx_lock(&loop->lock);
  while ((node = list_remove_head(&loop->wait_list))) {
    mtx_unlock(&loop->lock);
    async_wait_t* wait = node_to_wait(node);
    // Since the wait is being canceled, it would make sense to call zx_port_cancel()
    // here before invoking the callback to ensure that the waited-upon handle is
    // no longer attached to the port.  However, the port is about to be destroyed
    // so we can optimize that step away.
    async_loop_dispatch_wait(loop, wait, ZX_ERR_CANCELED, NULL);
    mtx_lock(&loop->lock);
  }
  while ((node = list_remove_head(&loop->due_list))) {
    mtx_unlock(&loop->lock);
    async_task_t* task = node_to_task(node);
    async_loop_dispatch_task(loop, task, ZX_ERR_CANCELED);
    mtx_lock(&loop->lock);
  }
  while ((node = list_remove_head(&loop->task_list))) {
    mtx_unlock(&loop->lock);
    async_task_t* task = node_to_task(node);
    async_loop_dispatch_task(loop, task, ZX_ERR_CANCELED);
    mtx_lock(&loop->lock);
  }
  while ((node = list_remove_head(&loop->irq_list))) {
    mtx_unlock(&loop->lock);
    async_irq_t* task = node_to_irq(node);
    async_loop_dispatch_irq(loop, task, ZX_ERR_CANCELED, NULL);
    mtx_lock(&loop->lock);
  }
  while ((node = list_remove_head(&loop->paged_vmo_list))) {
    mtx_unlock(&loop->lock);
    async_paged_vmo_t* paged_vmo = node_to_paged_vmo(node);
    // The loop owns the association between the pager and the VMO so when the
    // loop is shutting down, it is responsible for breaking that association
    // then notifying the callback that the wait has been canceled.
    async_loop_cancel_paged_vmo(paged_vmo);
    async_loop_dispatch_paged_vmo(loop, paged_vmo, ZX_ERR_CANCELED, NULL);
    mtx_lock(&loop->lock);
  }

  mtx_unlock(&loop->lock);
}

void async_loop_shutdown(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);

  async_loop_state_t prior_state =
      atomic_exchange_explicit(&loop->state, ASYNC_LOOP_SHUTDOWN, memory_order_acq_rel);
  if (prior_state == ASYNC_LOOP_SHUTDOWN)
    return;

  // Wake all worker threads, and wait for them to finish.
  //
  // If there is at least one worker thread present, it will cancel all
  // pending tasks.
  async_loop_wake_threads(loop);
  async_loop_join_threads(loop);

  // Cancel any remaining pending tasks on our queues.
  //
  // All tasks will have been cancelled by a worker thread, unless there
  // were none: in this case, we clear them here.
  async_loop_cancel_all(loop);

  if (loop->config.make_default_for_current_thread) {
    ZX_DEBUG_ASSERT_MSG(
        loop->config.default_accessors.getter() == &loop->dispatcher,
        "The default dispatcher for the current thread is different from the dispatcher "
        "of this async loop. "
        "If you used the kAsyncLoopConfigAttachToCurrentThread loop config, "
        "the loop must be created and destroyed on the same thread. "
        "Did you move the loop to a different thread?");
    loop->config.default_accessors.setter(NULL);
  }
}

zx_status_t async_loop_run(async_loop_t* loop, zx_time_t deadline, bool once) {
  ZX_DEBUG_ASSERT(loop);

  zx_status_t status;
  atomic_fetch_add_explicit(&loop->active_threads, 1u, memory_order_acq_rel);
  do {
    status = async_loop_run_once(loop, deadline);
  } while (status == ZX_OK && !once);
  atomic_fetch_sub_explicit(&loop->active_threads, 1u, memory_order_acq_rel);
  return status;
}

zx_status_t async_loop_run_until_idle(async_loop_t* loop) {
  zx_status_t status = async_loop_run(loop, 0, false);
  if (status == ZX_ERR_TIMED_OUT) {
    status = ZX_OK;
  }
  return status;
}

static zx_status_t async_loop_run_once(async_loop_t* loop, zx_time_t deadline) {
  async_loop_state_t state = atomic_load_explicit(&loop->state, memory_order_acquire);
  if (state == ASYNC_LOOP_SHUTDOWN)
    return ZX_ERR_BAD_STATE;
  if (state != ASYNC_LOOP_RUNNABLE)
    return ZX_ERR_CANCELED;

  zx_port_packet_t packet;
  zx_status_t status = zx_port_wait(loop->port, deadline, &packet);
  if (status != ZX_OK)
    return status;

  if (packet.key == KEY_CONTROL) {
    // Handle wake-up packets.
    if (packet.type == ZX_PKT_TYPE_USER)
      return ZX_OK;

    // Handle task timer expirations.
    if (packet.type == ZX_PKT_TYPE_SIGNAL_ONE && packet.signal.observed & ZX_TIMER_SIGNALED) {
      return async_loop_dispatch_tasks(loop);
    }
  } else {
    // Handle wait completion packets.
    if (packet.type == ZX_PKT_TYPE_SIGNAL_ONE) {
      async_wait_t* wait = (void*)(uintptr_t)packet.key;
      mtx_lock(&loop->lock);
      list_delete(wait_to_node(wait));
      mtx_unlock(&loop->lock);
      return async_loop_dispatch_wait(loop, wait, packet.status, &packet.signal);
    }

    // Handle queued user packets.
    if (packet.type == ZX_PKT_TYPE_USER) {
      async_receiver_t* receiver = (void*)(uintptr_t)packet.key;
      return async_loop_dispatch_packet(loop, receiver, packet.status, &packet.user);
    }

    // Handle guest bell trap packets.
    if (packet.type == ZX_PKT_TYPE_GUEST_BELL) {
      async_guest_bell_trap_t* trap = (void*)(uintptr_t)packet.key;
      return async_loop_dispatch_guest_bell_trap(loop, trap, packet.status, &packet.guest_bell);
    }

    // Handle interrupt packets.
    if (packet.type == ZX_PKT_TYPE_INTERRUPT) {
      async_irq_t* irq = (void*)(uintptr_t)packet.key;
      return async_loop_dispatch_irq(loop, irq, packet.status, &packet.interrupt);
    }
    // Handle pager packets.
    if (packet.type == ZX_PKT_TYPE_PAGE_REQUEST) {
      async_paged_vmo_t* paged_vmo = (void*)(uintptr_t)packet.key;
      return async_loop_dispatch_paged_vmo(loop, paged_vmo, packet.status, &packet.page_request);
    }
  }

  ZX_DEBUG_ASSERT(false);
  return ZX_ERR_INTERNAL;
}

async_dispatcher_t* async_loop_get_dispatcher(async_loop_t* loop) {
  // Note: The loop's implementation inherits from async_t so we can upcast to it.
  return (async_dispatcher_t*)loop;
}

async_loop_t* async_loop_from_dispatcher(async_dispatcher_t* async) { return (async_loop_t*)async; }

static zx_status_t async_loop_dispatch_guest_bell_trap(async_loop_t* loop,
                                                       async_guest_bell_trap_t* trap,
                                                       zx_status_t status,
                                                       const zx_packet_guest_bell_t* bell) {
  async_loop_invoke_prologue(loop);
  trap->handler((async_dispatcher_t*)loop, trap, status, bell);
  async_loop_invoke_epilogue(loop);
  return ZX_OK;
}

static zx_status_t async_loop_dispatch_wait(async_loop_t* loop, async_wait_t* wait,
                                            zx_status_t status, const zx_packet_signal_t* signal) {
  async_loop_invoke_prologue(loop);
  wait->handler((async_dispatcher_t*)loop, wait, status, signal);
  async_loop_invoke_epilogue(loop);
  return ZX_OK;
}

static zx_status_t async_loop_dispatch_irq(async_loop_t* loop, async_irq_t* irq, zx_status_t status,
                                           const zx_packet_interrupt_t* interrupt) {
  async_loop_invoke_prologue(loop);
  irq->handler((async_dispatcher_t*)loop, irq, status, interrupt);
  async_loop_invoke_epilogue(loop);
  return ZX_OK;
}

static zx_status_t async_loop_dispatch_tasks(async_loop_t* loop) {
  // Dequeue and dispatch one task at a time in case an earlier task wants
  // to cancel a later task which has also come due.  At most one thread
  // can dispatch tasks at any given moment (to preserve serial ordering).
  // Timer restarts are suppressed until we run out of tasks to dispatch.
  mtx_lock(&loop->lock);
  if (!loop->dispatching_tasks) {
    loop->dispatching_tasks = true;

    // Extract all of the tasks that are due into |due_list| for dispatch
    // unless we already have some waiting from a previous iteration which
    // we would like to process in order.
    list_node_t* node;
    if (list_is_empty(&loop->due_list)) {
      zx_time_t due_time = async_loop_now((async_dispatcher_t*)loop);
      list_node_t* tail = NULL;
      list_for_every(&loop->task_list, node) {
        if (node_to_task(node)->deadline > due_time)
          break;
        tail = node;
      }
      if (tail) {
        list_node_t* head = loop->task_list.next;
        loop->task_list.next = tail->next;
        tail->next->prev = &loop->task_list;
        loop->due_list.next = head;
        head->prev = &loop->due_list;
        loop->due_list.prev = tail;
        tail->next = &loop->due_list;
      }
    }

    // Dispatch all due tasks.  Note that they might be canceled concurrently
    // so we need to grab the lock during each iteration to fetch the next
    // item from the list.
    while ((node = list_remove_head(&loop->due_list))) {
      mtx_unlock(&loop->lock);

      // Invoke the handler.  Note that it might destroy itself.
      async_task_t* task = node_to_task(node);
      async_loop_dispatch_task(loop, task, ZX_OK);

      mtx_lock(&loop->lock);
      async_loop_state_t state = atomic_load_explicit(&loop->state, memory_order_acquire);
      if (state != ASYNC_LOOP_RUNNABLE)
        break;
    }

    loop->dispatching_tasks = false;
    loop->timer_armed = false;
    async_loop_restart_timer_locked(loop);
  }
  mtx_unlock(&loop->lock);
  return ZX_OK;
}

static void async_loop_dispatch_task(async_loop_t* loop, async_task_t* task, zx_status_t status) {
  // Invoke the handler.  Note that it might destroy itself.
  async_loop_invoke_prologue(loop);
  task->handler((async_dispatcher_t*)loop, task, status);
  async_loop_invoke_epilogue(loop);
}

static zx_status_t async_loop_dispatch_packet(async_loop_t* loop, async_receiver_t* receiver,
                                              zx_status_t status, const zx_packet_user_t* data) {
  // Invoke the handler.  Note that it might destroy itself.
  async_loop_invoke_prologue(loop);
  receiver->handler((async_dispatcher_t*)loop, receiver, status, data);
  async_loop_invoke_epilogue(loop);
  return ZX_OK;
}

static zx_status_t async_loop_dispatch_paged_vmo(async_loop_t* loop, async_paged_vmo_t* paged_vmo,
                                                 zx_status_t status,
                                                 const zx_packet_page_request_t* page_request) {
  // Invoke the handler.  Note that it might destroy itself.
  async_loop_invoke_prologue(loop);
  paged_vmo->handler((async_dispatcher_t*)loop, paged_vmo, status, page_request);
  async_loop_invoke_epilogue(loop);
  return ZX_OK;
}

void async_loop_quit(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);

  async_loop_state_t expected_state = ASYNC_LOOP_RUNNABLE;
  if (!atomic_compare_exchange_strong_explicit(&loop->state, &expected_state, ASYNC_LOOP_QUIT,
                                               memory_order_acq_rel, memory_order_acquire))
    return;

  async_loop_wake_threads(loop);
}

static void async_loop_wake_threads(async_loop_t* loop) {
  // Queue enough packets to awaken all active threads.
  // This is safe because any new threads which join the pool first increment the
  // active thread count then check the loop state, so the count we observe here
  // cannot be less than the number of threads which might be blocked in |port_wait|.
  // Issuing too many packets is also harmless.
  uint32_t n = atomic_load_explicit(&loop->active_threads, memory_order_acquire);
  for (uint32_t i = 0u; i < n; i++) {
    zx_port_packet_t packet = {.key = KEY_CONTROL, .type = ZX_PKT_TYPE_USER, .status = ZX_OK};
    zx_status_t status = zx_port_queue(loop->port, &packet);
    ZX_ASSERT_MSG(status == ZX_OK, "zx_port_queue: status=%d", status);
  }
}

zx_status_t async_loop_reset_quit(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);

  // Ensure that there are no active threads before resetting the quit state.
  // This check is inherently racy but not dangerously so.  It's mainly a
  // sanity check for client code so we can make a stronger statement about
  // how |async_loop_reset_quit()| is supposed to be used.
  uint32_t n = atomic_load_explicit(&loop->active_threads, memory_order_acquire);
  if (n != 0)
    return ZX_ERR_BAD_STATE;

  async_loop_state_t expected_state = ASYNC_LOOP_QUIT;
  if (atomic_compare_exchange_strong_explicit(&loop->state, &expected_state, ASYNC_LOOP_RUNNABLE,
                                              memory_order_acq_rel, memory_order_acquire)) {
    return ZX_OK;
  }

  async_loop_state_t state = atomic_load_explicit(&loop->state, memory_order_acquire);
  if (state == ASYNC_LOOP_RUNNABLE)
    return ZX_OK;
  return ZX_ERR_BAD_STATE;
}

async_loop_state_t async_loop_get_state(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);

  return atomic_load_explicit(&loop->state, memory_order_acquire);
}

zx_time_t async_loop_now(async_dispatcher_t* dispatcher) { return zx_clock_get_monotonic(); }

static zx_status_t async_loop_begin_wait(async_dispatcher_t* async, async_wait_t* wait) {
  async_loop_t* loop = (async_loop_t*)async;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(wait);

  mtx_lock(&loop->lock);
  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN) {
    mtx_unlock(&loop->lock);
    return ZX_ERR_BAD_STATE;
  }

  zx_status_t status =
      zx_object_wait_async(wait->object, loop->port, (uintptr_t)wait, wait->trigger, wait->options);
  if (status == ZX_OK) {
    list_add_head(&loop->wait_list, wait_to_node(wait));
  } else {
    ZX_ASSERT_MSG(status == ZX_ERR_ACCESS_DENIED, "zx_object_wait_async: status=%d", status);
  }

  mtx_unlock(&loop->lock);
  return status;
}

static zx_status_t async_loop_cancel_wait(async_dispatcher_t* async, async_wait_t* wait) {
  async_loop_t* loop = (async_loop_t*)async;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(wait);

  // Note: We need to process cancellations even while the loop is being
  // destroyed in case the client is counting on the handler not being
  // invoked again past this point.

  mtx_lock(&loop->lock);

  // First, confirm that the wait is actually pending.
  list_node_t* node = wait_to_node(wait);
  if (!list_in_list(node)) {
    mtx_unlock(&loop->lock);
    return ZX_ERR_NOT_FOUND;
  }

  // Next, cancel the wait.  This may be racing with another thread that
  // has read the wait's packet but not yet dispatched it.  So if we fail
  // to cancel then we assume we lost the race.
  zx_status_t status = zx_port_cancel(loop->port, wait->object, (uintptr_t)wait);
  if (status == ZX_OK) {
    list_delete(node);
  } else {
    ZX_ASSERT_MSG(status == ZX_ERR_NOT_FOUND, "zx_port_cancel: status=%d", status);
  }

  mtx_unlock(&loop->lock);
  return status;
}

static zx_status_t async_loop_post_task(async_dispatcher_t* async, async_task_t* task) {
  async_loop_t* loop = (async_loop_t*)async;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(task);

  mtx_lock(&loop->lock);
  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN) {
    mtx_unlock(&loop->lock);
    return ZX_ERR_BAD_STATE;
  }

  async_loop_insert_task_locked(loop, task);
  if (!loop->dispatching_tasks && task_to_node(task)->prev == &loop->task_list) {
    // Task inserted at head.  Earliest deadline changed.
    async_loop_restart_timer_locked(loop);
  }

  mtx_unlock(&loop->lock);
  return ZX_OK;
}

static zx_status_t async_loop_cancel_task(async_dispatcher_t* async, async_task_t* task) {
  async_loop_t* loop = (async_loop_t*)async;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(task);

  // Note: We need to process cancellations even while the loop is being
  // destroyed in case the client is counting on the handler not being
  // invoked again past this point.  Also, the task we're removing here
  // might be present in the dispatcher's |due_list| if it is pending
  // dispatch instead of in the loop's |task_list| as usual.  The same
  // logic works in both cases.

  mtx_lock(&loop->lock);
  list_node_t* node = task_to_node(task);
  if (!list_in_list(node)) {
    mtx_unlock(&loop->lock);
    return ZX_ERR_NOT_FOUND;
  }

  // Determine whether the head task was canceled and following task has
  // a later deadline.  If so, we will bump the timer along to that deadline.
  bool must_restart =
      !loop->dispatching_tasks && node->prev == &loop->task_list &&
      (node->next == &loop->task_list || node_to_task(node->next)->deadline > task->deadline);
  list_delete(node);
  if (must_restart)
    async_loop_restart_timer_locked(loop);

  mtx_unlock(&loop->lock);
  return ZX_OK;
}

static zx_status_t async_loop_queue_packet(async_dispatcher_t* async, async_receiver_t* receiver,
                                           const zx_packet_user_t* data) {
  async_loop_t* loop = (async_loop_t*)async;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(receiver);

  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN)
    return ZX_ERR_BAD_STATE;

  zx_port_packet_t packet = {.key = (uintptr_t)receiver, .type = ZX_PKT_TYPE_USER, .status = ZX_OK};
  if (data)
    packet.user = *data;
  return zx_port_queue(loop->port, &packet);
}

static zx_status_t async_loop_set_guest_bell_trap(async_dispatcher_t* async,
                                                  async_guest_bell_trap_t* trap, zx_handle_t guest,
                                                  zx_vaddr_t addr, size_t length) {
  async_loop_t* loop = (async_loop_t*)async;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(trap);

  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN)
    return ZX_ERR_BAD_STATE;

  zx_status_t status =
      zx_guest_set_trap(guest, ZX_GUEST_TRAP_BELL, addr, length, loop->port, (uintptr_t)trap);
  if (status != ZX_OK) {
    ZX_ASSERT_MSG(status == ZX_ERR_ACCESS_DENIED || status == ZX_ERR_ALREADY_EXISTS ||
                      status == ZX_ERR_INVALID_ARGS || status == ZX_ERR_OUT_OF_RANGE ||
                      status == ZX_ERR_WRONG_TYPE,
                  "zx_guest_set_trap: status=%d", status);
  }
  return status;
}

static zx_status_t async_loop_create_paged_vmo(async_dispatcher_t* async,
                                               async_paged_vmo_t* paged_vmo, uint32_t options,
                                               zx_handle_t pager, uint64_t vmo_size,
                                               zx_handle_t* vmo_out) {
  async_loop_t* loop = (async_loop_t*)async;
  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN) {
    return ZX_ERR_BAD_STATE;
  }

  zx_status_t status =
      zx_pager_create_vmo(pager, options, loop->port, (uintptr_t)paged_vmo, vmo_size, vmo_out);
  if (status != ZX_OK) {
    return status;
  }

  mtx_lock(&loop->lock);
  list_add_head(&loop->paged_vmo_list, paged_vmo_to_node(paged_vmo));
  mtx_unlock(&loop->lock);
  return ZX_OK;
}

static zx_status_t async_loop_detach_paged_vmo(async_dispatcher_t* async,
                                               async_paged_vmo_t* paged_vmo) {
  list_node_t* node = paged_vmo_to_node(paged_vmo);

  async_loop_t* loop = (async_loop_t*)async;
  mtx_lock(&loop->lock);

  if (!list_in_list(node)) {
    mtx_unlock(&loop->lock);
    return ZX_ERR_NOT_FOUND;
  }

  zx_status_t status = zx_pager_detach_vmo(paged_vmo->pager, paged_vmo->vmo);
  // Even on failure (maybe the VMO was already destroyed), remove the node from the list to
  // prevent a crash tearing down the list.

  // NOTE: the client owns the VMO and is responsible for freeing it.
  list_delete(node);
  mtx_unlock(&loop->lock);
  return status;
}

static zx_status_t async_loop_cancel_paged_vmo(async_paged_vmo_t* paged_vmo) {
  // This function gets called from the async loop shutdown path. The handler will not receive any
  // detach callbacks as the loop is shutting down. So explicitly detach the VMO from the pager.
  return zx_pager_detach_vmo(paged_vmo->pager, paged_vmo->vmo);
}

static void async_loop_insert_task_locked(async_loop_t* loop, async_task_t* task) {
  // TODO(https://fxbug.dev/42105840): We assume that tasks are inserted in quasi-monotonic order
  // and that insertion into the task queue will typically take no more than a few steps. If this
  // assumption proves false and the cost of insertion becomes a problem, we should consider using a
  // more efficient representation for maintaining order.
  list_node_t* node;
  for (node = loop->task_list.prev; node != &loop->task_list; node = node->prev) {
    if (task->deadline >= node_to_task(node)->deadline)
      break;
  }
  list_add_after(node, task_to_node(task));
}

static zx_time_t async_loop_next_deadline_locked(async_loop_t* loop) {
  if (list_is_empty(&loop->due_list)) {
    list_node_t* head = list_peek_head(&loop->task_list);
    if (!head)
      return ZX_TIME_INFINITE;
    async_task_t* task = node_to_task(head);
    if (task->deadline == ZX_TIME_INFINITE)
      return ZX_TIME_INFINITE;
    else
      return task->deadline;
  }
  // Fire now.
  return 0ULL;
}

static void async_loop_restart_timer_locked(async_loop_t* loop) {
  zx_status_t status;
  zx_time_t deadline = async_loop_next_deadline_locked(loop);

  if (deadline == ZX_TIME_INFINITE) {
    // Nothing is left on the queue to fire.
    if (loop->timer_armed) {
      status = zx_timer_cancel(loop->timer);
      ZX_ASSERT_MSG(status == ZX_OK, "zx_timer_cancel: status=%d", status);
      // ZX_ERR_NOT_FOUND can happen here when a pending timer fires and
      // the packet is picked up by port_wait in another thread but has
      // not reached dispatch.
      status = zx_port_cancel(loop->port, loop->timer, KEY_CONTROL);
      ZX_ASSERT_MSG(status == ZX_OK || status == ZX_ERR_NOT_FOUND, "zx_port_cancel: status=%d",
                    status);
      loop->timer_armed = false;
    }

    return;
  }

  status = zx_timer_set(loop->timer, deadline, 0);
  ZX_ASSERT_MSG(status == ZX_OK, "zx_timer_set: status=%d", status);

  if (!loop->timer_armed) {
    loop->timer_armed = true;
    status = zx_object_wait_async(loop->timer, loop->port, KEY_CONTROL, ZX_TIMER_SIGNALED, 0);
    ZX_ASSERT_MSG(status == ZX_OK, "zx_object_wait_async: status=%d", status);
  }
}

static void async_loop_invoke_prologue(async_loop_t* loop) {
  if (loop->config.prologue)
    loop->config.prologue(loop, loop->config.data);
}

static void async_loop_invoke_epilogue(async_loop_t* loop) {
  if (loop->config.epilogue)
    loop->config.epilogue(loop, loop->config.data);
}

static zx_status_t async_loop_bind_irq(async_dispatcher_t* dispatcher, async_irq_t* irq) {
  async_loop_t* loop = (async_loop_t*)dispatcher;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(irq);

  mtx_lock(&loop->lock);
  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN) {
    mtx_unlock(&loop->lock);
    return ZX_ERR_BAD_STATE;
  }

  zx_status_t status =
      zx_interrupt_bind(irq->object, loop->port, (uintptr_t)irq, ZX_INTERRUPT_BIND);
  if (status == ZX_OK) {
    list_add_head(&loop->irq_list, irq_to_node(irq));
  } else {
    ZX_ASSERT_MSG(status == ZX_ERR_ACCESS_DENIED, "zx_interrupt_bind (bind): status=%d", status);
  }

  mtx_unlock(&loop->lock);
  return status;
}

static zx_status_t async_loop_unbind_irq(async_dispatcher_t* dispatcher, async_irq_t* irq) {
  async_loop_t* loop = (async_loop_t*)dispatcher;
  ZX_DEBUG_ASSERT(loop);
  ZX_DEBUG_ASSERT(irq);

  if (atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN)
    return ZX_ERR_BAD_STATE;

  mtx_lock(&loop->lock);

  zx_status_t status =
      zx_interrupt_bind(irq->object, loop->port, (uintptr_t)irq, ZX_INTERRUPT_UNBIND);

  // ZX_ERR_CANCELED is returned if the interrupt has already been destroyed
  // before it's unbound.
  if (status == ZX_OK || status == ZX_ERR_CANCELED) {
    list_delete(irq_to_node(irq));
    status = ZX_OK;
  } else {
    ZX_ASSERT_MSG(status == ZX_ERR_ACCESS_DENIED, "zx_interrupt_bind (unbind): status=%d", status);
  }
  mtx_unlock(&loop->lock);
  return status;
}

static int async_loop_run_thread(void* data) {
  async_loop_t* loop = (async_loop_t*)data;
  if (loop->config.default_accessors.setter) {
    loop->config.default_accessors.setter(&loop->dispatcher);
  }
  async_loop_run(loop, ZX_TIME_INFINITE, false);

  // Determine if we are the last worker to finish.
  bool last_worker =
      atomic_fetch_sub_explicit(&loop->worker_threads, 1u, memory_order_acq_rel) == 1u;

  // If the thread exited due to shutdown and we are the last worker
  // thread to finish, start clearing out queues.
  if (last_worker &&
      atomic_load_explicit(&loop->state, memory_order_acquire) == ASYNC_LOOP_SHUTDOWN) {
    async_loop_cancel_all(loop);
  }

  return 0;
}

zx_status_t async_loop_start_thread(async_loop_t* loop, const char* name, thrd_t* out_thread) {
  ZX_DEBUG_ASSERT(loop);

  // This check is inherently racy.  The client should not be racing shutdown
  // with attempts to start new threads.  This is mainly a sanity check.
  async_loop_state_t state = atomic_load_explicit(&loop->state, memory_order_acquire);
  if (state == ASYNC_LOOP_SHUTDOWN)
    return ZX_ERR_BAD_STATE;

  thread_record_t* rec = calloc(1u, sizeof(thread_record_t));
  if (!rec)
    return ZX_ERR_NO_MEMORY;

  if (thrd_create_with_name(&rec->thread, async_loop_run_thread, loop, name) != thrd_success) {
    free(rec);
    return ZX_ERR_NO_MEMORY;
  }

  mtx_lock(&loop->lock);
  atomic_fetch_add_explicit(&loop->worker_threads, 1u, memory_order_acq_rel);
  list_add_tail(&loop->thread_list, &rec->node);
  mtx_unlock(&loop->lock);

  if (out_thread)
    *out_thread = rec->thread;
  return ZX_OK;
}

void async_loop_join_threads(async_loop_t* loop) {
  ZX_DEBUG_ASSERT(loop);

  mtx_lock(&loop->lock);
  for (;;) {
    thread_record_t* rec = (thread_record_t*)list_remove_head(&loop->thread_list);
    if (!rec)
      break;

    mtx_unlock(&loop->lock);
    thrd_t thread = rec->thread;
    free(rec);
    int result = thrd_join(thread, NULL);
    ZX_DEBUG_ASSERT(result == thrd_success);
    mtx_lock(&loop->lock);
  }
  mtx_unlock(&loop->lock);
}
