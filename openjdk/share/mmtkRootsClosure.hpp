#ifndef MMTK_OPENJDK_MMTK_ROOTS_CLOSURE_HPP
#define MMTK_OPENJDK_MMTK_ROOTS_CLOSURE_HPP

#include "memory/iterator.hpp"
#include "memory/universe.hpp"
#include "mmtk.h"
#include "oops/oop.hpp"
#include "oops/oop.inline.hpp"
#include "utilities/globalDefinitions.hpp"
#include <cstdio>
#include <cstdlib>

class MMTkRootsClosure : public OopClosure {
  SlotsClosure _slots_closure;
  void** _buffer;
  size_t _cap;
  size_t _cursor;

  template <class T>
  inline void do_oop_work(T* p, bool narrow) {
    T heap_oop = RawAccess<>::oop_load(p);
    if (!CompressedOops::is_null(heap_oop)) {
      const bool trace_heap_root_slots = std::getenv("MMTK_TRACE_HEAP_ROOT_SLOTS") != nullptr;
      const bool abort_on_heap_root_slot = std::getenv("MMTK_ABORT_ON_HEAP_ROOT_SLOT") != nullptr;
      if ((trace_heap_root_slots || abort_on_heap_root_slot) && Universe::heap()->is_in((void*)p)) {
        static int heap_root_slot_budget = 64;
        if (trace_heap_root_slots && heap_root_slot_budget-- > 0) {
          std::fprintf(
              stderr,
              "MMTK heap-resident root slot: slot=%p narrow=%d\n",
              (void*)p,
              narrow ? 1 : 0);
        }
        if (abort_on_heap_root_slot) {
          std::fprintf(
              stderr,
              "MMTK aborting on heap-resident root slot: slot=%p narrow=%d\n",
              (void*)p,
              narrow ? 1 : 0);
          std::fflush(stderr);
          std::abort();
        }
      }
      if (UseCompressedOops && !narrow) {
        guarantee((uintptr_t(p) & (1ull << 63)) == 0, "test");
        p = (T*) (uintptr_t(p) | (1ull << 63));
      }
      _buffer[_cursor++] = (void*) p;
      if (_cursor >= _cap) {
        flush();
      }
    }
  }

  void flush() {
    if (_cursor > 0) {
      NewBuffer buf = _slots_closure.invoke(_buffer, _cursor, _cap);
      _buffer = buf.buf;
      _cap = buf.cap;
      _cursor = 0;
    }
  }

public:
  MMTkRootsClosure(SlotsClosure slots_closure): _slots_closure(slots_closure), _cursor(0) {
    NewBuffer buf = slots_closure.invoke(NULL, 0, 0);
    _buffer = buf.buf;
    _cap = buf.cap;
  }

  ~MMTkRootsClosure() {
    if (_cursor > 0) flush();
    if (_buffer != NULL) {
      release_buffer(_buffer, _cursor, _cap);
    }
  }

  virtual void do_oop(oop* p)       { do_oop_work(p, false); }
  virtual void do_oop(narrowOop* p) { do_oop_work(p, true); }
};

class MMTkScanObjectClosure : public BasicOopIterateClosure {
  void* _trace;
  CLDToOopClosure follow_cld_closure;

  template <class T>
  void do_oop_work(T* p, bool narrow) {
    if (UseCompressedOops && !narrow) {
      guarantee((uintptr_t(p) & (1ull << 63)) == 0, "test");
      p = (T*) (uintptr_t(p) | (1ull << 63));
    }
  }

public:
  MMTkScanObjectClosure(void* trace): _trace(trace), follow_cld_closure(this, false) {}

  virtual void do_oop(oop* p)       { do_oop_work(p, false); }
  virtual void do_oop(narrowOop* p) { do_oop_work(p, true); }

  virtual bool do_metadata() {
    return true;
  }

  virtual void do_klass(Klass* k) {
  //  follow_cld_closure.do_cld(k->class_loader_data());
    // oop op = k->klass_holder();
    // oop new_op = (oop) trace_root_object(_trace, op);
    // guarantee(new_op == op, "trace_root_object returned a different value %p -> %p", op, new_op);
  }

  virtual void do_cld(ClassLoaderData* cld) {
    follow_cld_closure.do_cld(cld);
  }

  // StackChunk oop iteration may report embedded Method/nmethod metadata.  MMTk's
  // object-scanning path only needs the oop slots here; keeping these as no-ops avoids
  // BasicOopIterateClosure's debug aborts and matches the existing binding behavior of
  // not doing additional metadata tracing from this closure.
  virtual void do_method(Method* m) {}
  virtual void do_nmethod(nmethod* nm) {}

  virtual ReferenceIterationMode reference_iteration_mode() { return DO_FIELDS; }
  virtual bool idempotent() { return true; }
};

// class MMTkCLDClosure : public CLDClosure {
// public:
//   virtual void do_cld(ClassLoaderData* cld) {

//     printf("CLD: %p", p);
//   }
// };

#endif // MMTK_OPENJDK_MMTK_ROOTS_CLOSURE_HPP
