// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <lib/zircon-internal/macros.h>
#include <platform.h>
#include <pow2.h>
#include <stdio.h>
#include <stdlib.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <arch/interrupt.h>
#include <fbl/algorithm.h>
#include <kernel/auto_lock.h>
#include <kernel/cpu.h>
#include <kernel/event.h>
#include <kernel/mp.h>
#include <kernel/spinlock.h>
#include <kernel/thread.h>
#include <kernel/timer.h>
#include <ktl/atomic.h>
#include <ktl/iterator.h>
#include <ktl/optional.h>
#include <ktl/unique_ptr.h>

#include "tests.h"

#include <ktl/enforce.h>

static void timer_diag_cb(Timer* timer, zx_instant_mono_t now, void* arg) {
  Event* event = (Event*)arg;
  event->Signal();
}

static int timer_do_one_thread(void* arg) {
  Event event;
  Timer timer;

  const Deadline deadline = Deadline::after_mono(ZX_MSEC(10));
  timer.Set(deadline, timer_diag_cb, &event);
  event.Wait();

  printf("got timer on cpu %u\n", arch_curr_cpu_num());

  // Make sure the timer has fully completed before going out of scope.
  timer.Cancel();

  return 0;
}

static void timer_diag_all_cpus(void) {
  Thread* timer_threads[SMP_MAX_CPUS];
  uint max = arch_max_num_cpus();

  uint i;
  for (i = 0; i < max; i++) {
    char name[16];
    snprintf(name, sizeof(name), "timer %u\n", i);

    timer_threads[i] = Thread::Create(name, timer_do_one_thread, NULL, DEFAULT_PRIORITY);
    DEBUG_ASSERT_MSG(timer_threads[i] != NULL, "failed to create thread for cpu %u\n", i);
    timer_threads[i]->SetCpuAffinity(cpu_num_to_mask(i));
    timer_threads[i]->Resume();
  }
  for (i = 0; i < max; i++) {
    zx_status_t status = timer_threads[i]->Join(NULL, ZX_TIME_INFINITE);
    DEBUG_ASSERT_MSG(status == ZX_OK, "failed to join thread for cpu %u: %d\n", i, status);
  }
}

static void timer_diag_cb2(Timer* timer, zx_instant_mono_t now, void* arg) {
  auto timer_count = static_cast<ktl::atomic<size_t>*>(arg);
  timer_count->fetch_add(1);
  Thread::Current::preemption_state().PreemptSetPending();
}

static void timer_diag_coalescing(TimerSlack slack, const zx_instant_mono_t* deadline,
                                  const zx_duration_mono_t* expected_adj, size_t count) {
  printf("testing coalsecing mode %u\n", slack.mode());

  ktl::atomic<size_t> timer_count(0);

  fbl::AllocChecker ac;
  auto timers = ktl::unique_ptr<Timer[]>(new (&ac) Timer[count]);
  if (!ac.check()) {
    printf("\n!! failed to allocate %zu timers\n", count);
    return;
  }

  printf("       orig         new       adjustment\n");
  for (size_t ix = 0; ix != count; ++ix) {
    const Deadline dl(deadline[ix], slack);
    timers[ix].Set(dl, timer_diag_cb2, &timer_count);
    printf("[%zu] %" PRIi64 "  -> %" PRIi64 ", %" PRIi64 "\n", ix, dl.when(),
           timers[ix].scheduled_time_for_test(ZX_CLOCK_MONOTONIC), timers[ix].slack_for_test());

    if (timers[ix].slack_for_test() != expected_adj[ix]) {
      printf("\n!! unexpected adjustment! expected %" PRIi64 "\n", expected_adj[ix]);
    }
  }

  // Wait for the timers to fire.
  while (timer_count.load() != count) {
    Thread::Current::Sleep(current_mono_time() + ZX_MSEC(5));
  }

  // Cancel all the timers prior to going out of scope
  for (size_t i = 0; i < count; i++) {
    timers[i].Cancel();
  }
}

static void timer_diag_coalescing_center(void) {
  zx_instant_mono_t when = current_mono_time() + ZX_MSEC(1);
  zx_duration_mono_t off = ZX_USEC(10);
  TimerSlack slack = {2u * off, TIMER_SLACK_CENTER};

  const zx_instant_mono_t deadline[] = {
      when + (6u * off),  // non-coalesced, adjustment = 0
      when,               // non-coalesced, adjustment = 0
      when - off,         // coalesced with [1], adjustment = 10u
      when - (3u * off),  // non-coalesced, adjustment = 0
      when + off,         // coalesced with [1], adjustment = -10u
      when + (3u * off),  // non-coalesced, adjustment = 0
      when + (5u * off),  // coalesced with [0], adjustment = 10u
      when - (3u * off),  // non-coalesced, same as [3], adjustment = 0
  };

  const zx_duration_mono_t expected_adj[ktl::size(deadline)] = {
      0, 0, ZX_USEC(10), 0, -ZX_USEC(10), 0, ZX_USEC(10), 0};

  timer_diag_coalescing(slack, deadline, expected_adj, ktl::size(deadline));
}

static void timer_diag_coalescing_late(void) {
  zx_instant_mono_t when = current_mono_time() + ZX_MSEC(1);
  zx_duration_mono_t off = ZX_USEC(10);
  TimerSlack slack = {3u * off, TIMER_SLACK_LATE};

  const zx_instant_mono_t deadline[] = {
      when + off,         // non-coalesced, adjustment = 0
      when + (2u * off),  // non-coalesced, adjustment = 0
      when - off,         // coalesced with [0], adjustment = 20u
      when - (3u * off),  // non-coalesced, adjustment = 0
      when + (3u * off),  // non-coalesced, adjustment = 0
      when + (2u * off),  // non-coalesced, same as [1]
      when - (4u * off),  // coalesced with [3], adjustment = 10u
  };

  const zx_duration_mono_t expected_adj[ktl::size(deadline)] = {0, 0, ZX_USEC(20), 0,
                                                                0, 0, ZX_USEC(10)};

  timer_diag_coalescing(slack, deadline, expected_adj, ktl::size(deadline));
}

static void timer_diag_coalescing_early(void) {
  zx_instant_mono_t when = current_mono_time() + ZX_MSEC(1);
  zx_duration_mono_t off = ZX_USEC(10);
  TimerSlack slack = {3u * off, TIMER_SLACK_EARLY};

  const zx_instant_mono_t deadline[] = {
      when,               // non-coalesced, adjustment = 0
      when + (2u * off),  // coalesced with [0], adjustment = -20u
      when - off,         // non-coalesced, adjustment = 0
      when - (3u * off),  // non-coalesced, adjustment = 0
      when + (4u * off),  // non-coalesced, adjustment = 0
      when + (5u * off),  // coalesced with [4], adjustment = -10u
      when - (2u * off),  // coalesced with [3], adjustment = -10u
  };

  const zx_duration_mono_t expected_adj[ktl::size(deadline)] = {0, -ZX_USEC(20), 0,           0,
                                                                0, -ZX_USEC(10), -ZX_USEC(10)};

  timer_diag_coalescing(slack, deadline, expected_adj, ktl::size(deadline));
}

static void timer_far_deadline(void) {
  Event event;
  Timer timer;

  const Deadline deadline = Deadline::no_slack(ZX_TIME_INFINITE - 5);
  timer.Set(deadline, timer_diag_cb, &event);
  zx_status_t st = event.WaitDeadline(current_mono_time() + ZX_MSEC(100), Interruptible::No);
  if (st != ZX_ERR_TIMED_OUT) {
    printf("error: unexpected timer fired!\n");
  } else {
    timer.Cancel();
  }
}

// Print timer diagnostics for manual review.
int timer_diag(int, const cmd_args*, uint32_t) {
  timer_diag_coalescing_center();
  timer_diag_coalescing_late();
  timer_diag_coalescing_early();
  timer_diag_all_cpus();
  timer_far_deadline();
  return 0;
}

struct timer_stress_args {
  ktl::atomic<int> timer_stress_done;
  ktl::atomic<uint64_t> num_set;
  ktl::atomic<uint64_t> num_fired;
};

static void timer_stress_cb(Timer* t, zx_instant_mono_t now, void* void_arg) {
  timer_stress_args* args = reinterpret_cast<timer_stress_args*>(void_arg);
  args->num_fired++;
}

// Returns a random duration between 0 and max (inclusive).
static zx_duration_mono_t rand_duration(zx_duration_mono_t max) {
  return (zx_duration_mul_int64(max, rand())) / RAND_MAX;
}

static int timer_stress_worker(void* void_arg) {
  timer_stress_args* args = reinterpret_cast<timer_stress_args*>(void_arg);
  while (!args->timer_stress_done.load()) {
    // Create a timer on either the monotonic or boot timeline.
    // The timeline will be chosen randomly.
    const zx_clock_t timeline = rand() % 2 == 0 ? ZX_CLOCK_MONOTONIC : ZX_CLOCK_BOOT;
    const zx_duration_t timer_duration = rand_duration(ZX_MSEC(5));
    const Deadline deadline = timeline == ZX_CLOCK_MONOTONIC ? Deadline::after_mono(timer_duration)
                                                             : Deadline::after_boot(timer_duration);
    Timer t{timeline};

    // Set a timer, then switch to a different CPU to ensure we race with it.
    {
      InterruptDisableGuard block_interrupts;
      cpu_num_t timer_cpu = arch_curr_cpu_num();
      t.Set(deadline, timer_stress_cb, void_arg);
      Thread::Current::Get()->SetCpuAffinity(~cpu_num_to_mask(timer_cpu));
      DEBUG_ASSERT(arch_curr_cpu_num() != timer_cpu);
    }

    // We're now running on something other than timer_cpu.

    args->num_set++;

    // Sleep for the timer duration so that this thread's timer_cancel races with the timer
    // callback. We want to race to ensure there are no synchronization or memory visibility
    // issues. Note that we will not race if the system suspends while we sleep, so we must
    // ensure that we do not suspend.
    Thread::Current::SleepRelative(timer_duration);
    t.Cancel();
  }
  return 0;
}

static unsigned get_num_cpus_online() {
  unsigned count = 0;
  cpu_mask_t online = mp_get_online_mask();
  while (online) {
    online >>= 1;
    ++count;
  }
  return count;
}

// timer_stress is a simple stress test intended to flush out bugs in kernel timers.
int timer_stress(int argc, const cmd_args* argv, uint32_t) {
  if (argc < 2) {
    printf("not enough args\n");
    printf("usage: %s <num seconds>\n", argv[0].str);
    return ZX_ERR_INTERNAL;
  }

  // We need 2 or more CPUs for this test.
  if (get_num_cpus_online() < 2) {
    printf("not enough online cpus\n");
    return ZX_ERR_INTERNAL;
  }

  timer_stress_args args{};

  Thread* threads[256];
  for (auto& thread : threads) {
    thread = Thread::Create("timer-stress-worker", &timer_stress_worker, &args, DEFAULT_PRIORITY);
  }

  printf("running for %zu seconds\n", argv[1].u);
  for (const auto& thread : threads) {
    thread->Resume();
  }

  Thread::Current::SleepRelative(ZX_SEC(argv[1].u));
  args.timer_stress_done.store(1);

  for (const auto& thread : threads) {
    thread->Join(nullptr, ZX_TIME_INFINITE);
  }

  printf("timer stress done; timer set %zu, timer fired %zu\n", args.num_set.load(),
         args.num_fired.load());
  return 0;
}

struct timer_args {
  ktl::atomic<int> result;
  ktl::atomic<int> timer_fired;
  ktl::atomic<int> remaining;
  ktl::atomic<int> wait;
  DECLARE_SPINLOCK_WITH_TYPE(timer_args, MonitoredSpinLock) lock;
};

static void timer_cb(Timer*, zx_instant_mono_t now, void* void_arg) {
  timer_args* arg = reinterpret_cast<timer_args*>(void_arg);
  arg->timer_fired.store(1);
}

// Set a timer and cancel it before the deadline has elapsed.
static bool cancel_before_deadline() {
  BEGIN_TEST;
  timer_args arg{};
  Timer t;
  const Deadline deadline = Deadline::after_mono(ZX_HOUR(5));
  t.Set(deadline, timer_cb, &arg);
  ASSERT_TRUE(t.Cancel());
  ASSERT_FALSE(arg.timer_fired.load());
  END_TEST;
}

// Set a timer and cancel it after it has fired.
static bool cancel_after_fired() {
  BEGIN_TEST;
  timer_args arg{};
  Timer t;
  const Deadline deadline = Deadline::no_slack(current_mono_time());
  t.Set(deadline, timer_cb, &arg);
  while (!arg.timer_fired.load()) {
  }
  ASSERT_FALSE(t.Cancel());
  END_TEST;
}

static void timer_cancel_cb(Timer* t, zx_instant_mono_t now, void* void_arg) {
  timer_args* arg = reinterpret_cast<timer_args*>(void_arg);
  arg->result.store(t->Cancel());
  arg->timer_fired.store(1);
}

// Set a timer and cancel it from its own callback.
static bool cancel_from_callback() {
  BEGIN_TEST;
  timer_args arg{};
  arg.result = 1;
  Timer t;
  const Deadline deadline = Deadline::no_slack(current_mono_time());
  t.Set(deadline, timer_cancel_cb, &arg);
  while (!arg.timer_fired.load()) {
  }
  ASSERT_FALSE(t.Cancel());
  ASSERT_FALSE(arg.result);
  END_TEST;
}

static void timer_set_cb(Timer* t, zx_instant_mono_t now, void* void_arg) {
  timer_args* arg = reinterpret_cast<timer_args*>(void_arg);
  if (arg->remaining.fetch_sub(1) >= 1) {
    const Deadline deadline = Deadline::after_mono(ZX_USEC(10));
    t->Set(deadline, timer_set_cb, void_arg);
  }
}

// Set a timer that re-sets itself from its own callback.
static bool set_from_callback() {
  BEGIN_TEST;
  timer_args arg{};
  arg.remaining = 5;
  Timer t;
  const Deadline deadline = Deadline::no_slack(current_mono_time());
  t.Set(deadline, timer_set_cb, &arg);
  while (arg.remaining.load() > 0) {
  }

  // We cannot assert the return value below because we don't know if the last timer has fired.
  t.Cancel();

  END_TEST;
}

static void timer_trylock_cb(Timer* t, zx_instant_mono_t now, void* void_arg) {
  timer_args* arg = reinterpret_cast<timer_args*>(void_arg);
  arg->timer_fired.store(1);
  while (arg->wait.load()) {
  }

  int result = t->TrylockOrCancel(&arg->lock.lock());
  if (!result) {
    arg->lock.lock().Release();
  }

  arg->result.store(result);
}

// See that timer_trylock_or_cancel spins until the timer is canceled.
static bool trylock_or_cancel_canceled() {
  BEGIN_TEST;

#if defined(__x86_64__)
  // TODO(https://fxbug.dev/42166211): Test is disabled because it can deadlock with TLB
  // invalidation, which uses synchronous IPIs.
  printf("test is disabled on x86, see https://fxbug.dev/42166211\n");
  END_TEST;
#endif

  // We need 2 or more CPUs for this test.
  if (get_num_cpus_online() < 2) {
    printf("skipping test trylock_or_cancel_canceled, not enough online cpus\n");
    return true;
  }

  timer_args arg{};
  Timer t;

  arg.wait = 1;

  interrupt_saved_state_t int_state = arch_interrupt_save();

  cpu_num_t timer_cpu = arch_curr_cpu_num();
  const Deadline deadline = Deadline::after_mono(ZX_USEC(100));
  t.Set(deadline, timer_trylock_cb, &arg);

  // The timer is set to run on timer_cpu, switch to a different CPU, acquire the spinlock then
  // signal the callback to proceed.
  Thread::Current::Get()->SetCpuAffinity(~cpu_num_to_mask(timer_cpu));
  DEBUG_ASSERT(arch_curr_cpu_num() != timer_cpu);

  arch_interrupt_restore(int_state);

  {
    Guard<MonitoredSpinLock, IrqSave> guard{&arg.lock, SOURCE_TAG};

    while (!arg.timer_fired.load()) {
    }

    // Callback should now be running. Tell it to stop waiting and start trylocking.
    arg.wait.store(0);

    // See that timer_cancel returns false indicating that the timer ran.
    ASSERT_FALSE(t.Cancel());
  }

  // See that the timer failed to acquire the lock.
  ASSERT_TRUE(arg.result);
  END_TEST;
}

// See that timer_trylock_or_cancel acquires the lock when the holder releases it.
static bool trylock_or_cancel_get_lock() {
  BEGIN_TEST;

#if defined(__x86_64__)
  // TODO(https://fxbug.dev/42166211): Test is disabled because it can deadlock with TLB
  // invalidation, which uses synchronous IPIs.
  printf("test is disabled on x86, see https://fxbug.dev/42166211\n");
  END_TEST;
#endif

  // We need 2 or more CPUs for this test.
  if (get_num_cpus_online() < 2) {
    printf("skipping test trylock_or_cancel_get_lock, not enough online cpus\n");
    return true;
  }

  timer_args arg{};
  Timer t;

  arg.wait = 1;

  interrupt_saved_state_t int_state = arch_interrupt_save();

  cpu_num_t timer_cpu = arch_curr_cpu_num();
  const Deadline deadline = Deadline::after_mono(ZX_USEC(100));
  t.Set(deadline, timer_trylock_cb, &arg);
  // The timer is set to run on timer_cpu, switch to a different CPU, acquire the spinlock then
  // signal the callback to proceed.
  Thread::Current::Get()->SetCpuAffinity(~cpu_num_to_mask(timer_cpu));
  DEBUG_ASSERT(arch_curr_cpu_num() != timer_cpu);

  arch_interrupt_restore(int_state);

  {
    Guard<MonitoredSpinLock, IrqSave> guard{&arg.lock, SOURCE_TAG};

    while (!arg.timer_fired.load()) {
    }

    // Callback should now be running. Tell it to stop waiting and start trylocking.
    arg.wait.store(0);
  }

  // See that timer_cancel returns false indicating that the timer ran.
  ASSERT_FALSE(t.Cancel());

  // Note, we cannot assert the value of arg.result. We have both released the lock and canceled
  // the timer, but we don't know which of these events the timer observed first.

  END_TEST;
}

static bool print_timer_queues() {
  BEGIN_TEST;

  // Allocate a bunch of timers and a small buffer.  Set the timers then see that |PrintTimerQueues|
  // doesn't overflow the buffer.
  constexpr size_t kNumTimers = 1000;
  fbl::AllocChecker ac;
  auto timers = ktl::unique_ptr<Timer[]>(new (&ac) Timer[kNumTimers]);
  ASSERT_TRUE(ac.check());
  constexpr size_t kBufferSize = 4096;
  auto buffer = ktl::unique_ptr<char[]>(new (&ac) char[kBufferSize]);
  ASSERT_TRUE(ac.check());
  // Fill the buffer with a pattern so we can detect overflow.
  memset(buffer.get(), 'X', kBufferSize);

  for (size_t i = 0; i < kNumTimers; ++i) {
    timers[i].Set(Deadline::infinite(), [](Timer*, zx_instant_mono_t, void*) {}, nullptr);
  }
  auto cleanup = fit::defer([&]() {
    for (size_t i = 0; i < kNumTimers; ++i) {
      timers[i].Cancel();
    }
  });

  // Tell |PrintTimerQueues| the buffer is one less than it really is.
  TimerQueue::PrintTimerQueues(buffer.get(), kBufferSize - 1);

  // See that our sentinel was not overwritten.
  ASSERT_EQ('X', buffer[kBufferSize - 1]);

  // See that a null terminator was written to the last available position.
  ASSERT_EQ(0, buffer[kBufferSize - 2]);

  END_TEST;
}

static bool deadline_after() {
  BEGIN_TEST;

  ktl::array<ktl::optional<TimerSlack>, 5> kSlackModes{
      ktl::nullopt,        // nullopt is used for testing the default mode (should be "none").
      TimerSlack::none(),  // an explicit test of "none"
      TimerSlack(ZX_USEC(100), TIMER_SLACK_CENTER),
      TimerSlack(ZX_USEC(200), TIMER_SLACK_EARLY),
      TimerSlack(ZX_USEC(200), TIMER_SLACK_LATE),
  };

  // Test to make sure that a relative timeout which is an infinite amount of
  // time from now produces an infinite deadline.
  for (const auto& slack : kSlackModes) {
    Deadline deadline = slack.has_value() ? Deadline::after_mono(ZX_TIME_INFINITE, slack.value())
                                          : Deadline::after_mono(ZX_TIME_INFINITE);
    ASSERT_EQ(ZX_TIME_INFINITE, deadline.when());

    // Default slack should be "none"
    const TimerSlack& expected = slack.has_value() ? slack.value() : TimerSlack::none();
    ASSERT_EQ(expected.amount(), deadline.slack().amount());
    ASSERT_EQ(expected.mode(), deadline.slack().mode());
  }

  // While we cannot control the precise deadline which will be produced from
  // our call to Deadline::after, we _can_ bound the range it might exist in.
  // Test for this as well.
  for (const auto& slack : kSlackModes) {
    constexpr zx_duration_mono_t kTimeout = ZX_MSEC(10);
    zx_instant_mono_t before = zx_time_add_duration(current_mono_time(), kTimeout);
    Deadline deadline = slack.has_value() ? Deadline::after_mono(kTimeout, slack.value())
                                          : Deadline::after_mono(kTimeout);
    zx_instant_mono_t after = zx_time_add_duration(current_mono_time(), kTimeout);
    ASSERT_LE(before, deadline.when());
    ASSERT_GE(after, deadline.when());

    // Default slack should be "none"
    const TimerSlack& expected = slack.has_value() ? slack.value() : TimerSlack::none();
    ASSERT_EQ(expected.amount(), deadline.slack().amount());
    ASSERT_EQ(expected.mode(), deadline.slack().mode());
  }

  END_TEST;
}

static bool test_timer_current_mono_and_boot_ticks() {
  BEGIN_TEST;

  // Get the current monotonic and boot ticks. This should occur prior to our observation of both
  // below, providing us with a lower bound on those values.
  zx_instant_boot_ticks_t boot_before = timer_current_boot_ticks();
  zx_instant_mono_ticks_t mono_before = timer_current_mono_ticks();

  // Perform a synchronized read of the monotonic and boot ticks.
  CurrentTicksObservation obs = timer_current_mono_and_boot_ticks();

  // Get the current monotonic and boot ticks. This should occur after our observation of both
  // above, providing us with an upper bound on those values.
  zx_instant_mono_ticks_t mono_after = timer_current_mono_ticks();
  zx_instant_boot_ticks_t boot_after = timer_current_boot_ticks();

  // Ensure that the monotonic ticks are less than or equal to the boot ticks.
  ASSERT_LE(obs.mono_now, obs.boot_now);

  // Ensure that our observations are monotonic, meaning that they are greater than or equal to our
  // before observations and less than or equal to our after observations.
  ASSERT_GE(obs.mono_now, mono_before);
  ASSERT_GE(obs.boot_now, boot_before);
  ASSERT_LE(obs.mono_now, mono_after);
  ASSERT_LE(obs.boot_now, boot_after);

  END_TEST;
}

static bool mono_to_raw_ticks_overflow() {
  BEGIN_TEST;

  // Verify that converting ZX_TIME_INFINITE and ZX_TIME_INFINTE - 1 returns ZX_TIME_INFINITE
  // instead of overflowing.
  ktl::optional<zx_ticks_t> raw_ticks = timer_convert_mono_to_raw_ticks(ZX_TIME_INFINITE);
  ASSERT_TRUE(raw_ticks.has_value());
  ASSERT_EQ(raw_ticks.value(), ZX_TIME_INFINITE);

  raw_ticks = timer_convert_mono_to_raw_ticks(ZX_TIME_INFINITE - 1);
  ASSERT_TRUE(raw_ticks.has_value());
  ASSERT_GE(raw_ticks.value(), ZX_TIME_INFINITE - 1);

  // Verify that 0 gives us a raw ticks greater than or equal to 0, as the conversion function
  // should add an offset that is greater than or equal to 0.
  raw_ticks = timer_convert_mono_to_raw_ticks(0);
  ASSERT_TRUE(raw_ticks.has_value());
  ASSERT_GE(raw_ticks.value(), 0);

  // Verify that ZX_TIME_INFINITE_PAST and ZX_TIME_INFINITE_PAST + 1 return negative numbers,
  // since the mono_ticks_modifier should be much smaller than this value.
  raw_ticks = timer_convert_mono_to_raw_ticks(ZX_TIME_INFINITE_PAST);
  ASSERT_TRUE(raw_ticks.has_value());
  ASSERT_LT(raw_ticks.value(), 0);

  raw_ticks = timer_convert_mono_to_raw_ticks(ZX_TIME_INFINITE_PAST + 1);
  ASSERT_TRUE(raw_ticks.has_value());
  ASSERT_LT(raw_ticks.value(), 0);

  END_TEST;
}

// Ensure that a boot timer fires.
static bool boot_timer() {
  BEGIN_TEST;
  timer_args arg{};
  Timer t{ZX_CLOCK_BOOT};
  const Deadline deadline = Deadline::no_slack(current_boot_time());
  t.Set(deadline, timer_cb, &arg);
  while (!arg.timer_fired.load()) {
  }
  END_TEST;
}

// Guarantee montonicity of the monotonic timeline across multiple threads on different CPUs.
static bool check_monotonicity_mono() {
  BEGIN_TEST;

  // Test state shared across all threads.
  struct TestState {
    ktl::atomic<bool> test_started;
    ktl::atomic<zx_instant_mono_t> previous_time;
    const zx_instant_boot_t test_deadline;
  };
  TestState test_state = {
      .test_started = false,
      .previous_time = current_mono_time(),
      .test_deadline = zx_time_add_duration(current_boot_time(), zx_duration_from_sec(10)),
  };

  // Create a reader and writer per CPU in the system.
  const size_t kNumWriters = arch_max_num_cpus();
  const size_t kNumReaders = kNumWriters;
  const size_t kNumThreads = kNumReaders + kNumWriters;
  fbl::AllocChecker ac;
  ktl::unique_ptr<Thread*[]> threads = ktl::make_unique<Thread*[]>(&ac, kNumThreads);
  ASSERT_TRUE(ac.check());

  // The reader routine reads the previously seen time, gets the current time, and verifies that the
  // latter is greater than or equal to the former.
  auto reader = [](void* arg) -> int {
    TestState* ts = reinterpret_cast<TestState*>(arg);
    while (!ts->test_started.load()) {
    }
    while (current_boot_time() <= ts->test_deadline) {
      const zx_instant_mono_t prev = ts->previous_time.load(ktl::memory_order_acquire);
      const zx_instant_mono_t now = current_mono_time();
      DEBUG_ASSERT_MSG(now >= prev, "Time was not monotonic: Now: %ld, Previous: %ld\n", now, prev);
    }
    return 0;
  };

  // The writer routine gets the current time and updates the previous time to that value.
  auto writer = [](void* arg) -> int {
    TestState* ts = reinterpret_cast<TestState*>(arg);
    while (!ts->test_started.load()) {
    }
    while (current_boot_time() <= ts->test_deadline) {
      const zx_instant_mono_t now = current_mono_time();
      ts->previous_time.store(now, ktl::memory_order_release);
    }
    return 0;
  };

  // Create all of the reader threads.
  for (size_t i = 0; i < kNumReaders; i++) {
    threads[i] = Thread::Create("monotonicity_test_reader", reader, &test_state, DEFAULT_PRIORITY);
    ASSERT_NONNULL(threads[i], "Thread::Create failed for reader");
    threads[i]->Resume();
  }

  // Create all of the writer threads.
  for (size_t i = kNumReaders; i < kNumThreads; i++) {
    threads[i] = Thread::Create("monotonicity_test_writer", writer, &test_state, DEFAULT_PRIORITY);
    ASSERT_NONNULL(threads[i], "Thread::Create failed for writer");
    threads[i]->Resume();
  }

  // Start all of the threads and wait for them to complete.
  test_state.test_started.store(true);
  for (size_t i = 0; i < kNumThreads; i++) {
    int ret = -1;
    ASSERT_OK(threads[i]->Join(&ret, ZX_TIME_INFINITE));
    ASSERT_EQ(0, ret);
  }

  END_TEST;
}

UNITTEST_START_TESTCASE(timer_tests)
UNITTEST("cancel_before_deadline", cancel_before_deadline)
UNITTEST("cancel_after_fired", cancel_after_fired)
UNITTEST("cancel_from_callback", cancel_from_callback)
UNITTEST("set_from_callback", set_from_callback)
UNITTEST("trylock_or_cancel_canceled", trylock_or_cancel_canceled)
UNITTEST("trylock_or_cancel_get_lock", trylock_or_cancel_get_lock)
UNITTEST("print_timer_queue", print_timer_queues)
UNITTEST("Deadline::after", deadline_after)
UNITTEST("mono_to_raw_ticks_overflow", mono_to_raw_ticks_overflow)
UNITTEST("boot_timer", boot_timer)
UNITTEST("test_timer_current_mono_and_boot_ticks", test_timer_current_mono_and_boot_ticks)
UNITTEST("check_monotonicity_mono", check_monotonicity_mono)
UNITTEST_END_TESTCASE(timer_tests, "timer", "timer tests")
