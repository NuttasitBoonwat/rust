use traits;
use hir::def_id::DefId;
use ty::subst::Substs;
use ty::{self, Ty};
use syntax::codemap::{DUMMY_SP, Span};
use syntax::ast::{DUMMY_NODE_ID, Mutability};

use super::{EvalResult, EvalContext, eval_context, MemoryPointer, Value, PrimVal,
            Machine};

impl<'a, 'tcx, M: Machine<'tcx>> EvalContext<'a, 'tcx, M> {
    /// Attempts to resolve an obligation to a vtable.. The result is
    /// a shallow vtable resolution -- meaning that we do not
    /// (necessarily) resolve all nested obligations on the impl. Note
    /// that type check should guarantee to us that all nested
    /// obligations *could be* resolved if we wanted to.
    /// Assumes that this is run after the entire crate has been successfully type-checked.
    pub fn trans_fulfill_obligation(&self,
                                    span: Span,
                                    param_env: ty::ParamEnv<'tcx>,
                                    trait_ref: ty::PolyTraitRef<'tcx>)
                                    -> EvalResult<'tcx, traits::Vtable<'tcx, ()>>
    {
        // Remove any references to regions; this helps improve caching.
        let trait_ref = self.tcx.erase_regions(&trait_ref);

        debug!("trans::fulfill_obligation(trait_ref={:?}, def_id={:?})",
                (param_env, trait_ref), trait_ref.def_id());

        // Do the initial selection for the obligation. This yields the
        // shallow result we are looking for -- that is, what specific impl.
        self.tcx.infer_ctxt().enter(|infcx| {
            let mut selcx = traits::SelectionContext::new(&infcx);

            let obligation_cause = traits::ObligationCause::misc(span,
                                                            DUMMY_NODE_ID);
            let obligation = traits::Obligation::new(obligation_cause,
                                                param_env,
                                                trait_ref.to_poly_trait_predicate());

            let selection = match selcx.select(&obligation) {
                Ok(Some(selection)) => selection,
                Ok(None) => {
                    // Ambiguity can happen when monomorphizing during trans
                    // expands to some humongo type that never occurred
                    // statically -- this humongo type can then overflow,
                    // leading to an ambiguous result. So report this as an
                    // overflow bug, since I believe this is the only case
                    // where ambiguity can result.
                    debug!("Encountered ambiguity selecting `{:?}` during trans, \
                            presuming due to overflow",
                            trait_ref);
                    self.tcx.sess.span_fatal(span,
                                        "reached the recursion limit during monomorphization \
                                            (selection ambiguity)");
                }
                Err(traits::SelectionError::Unimplemented) => {
                    return err!(UnimplementedTraitSelection);
                }
                Err(e) => {
                    span_bug!(span, "Encountered error `{:?}` selecting `{:?}` during trans",
                                e, trait_ref)
                }
            };

            debug!("fulfill_obligation: selection={:?}", selection);

            // Currently, we use a fulfillment context to completely resolve
            // all nested obligations. This is because they can inform the
            // inference of the impl's type parameters.
            let mut fulfill_cx = traits::FulfillmentContext::new();
            let vtable = selection.map(|predicate| {
                debug!("fulfill_obligation: register_predicate_obligation {:?}", predicate);
                fulfill_cx.register_predicate_obligation(&infcx, predicate);
            });
            let vtable = infcx.drain_fulfillment_cx_or_panic(span, &mut fulfill_cx, &vtable);

            info!("Cache miss: {:?} => {:?}", trait_ref, vtable);
            Ok(vtable)
        })
    }
    /// Creates a dynamic vtable for the given type and vtable origin. This is used only for
    /// objects.
    ///
    /// The `trait_ref` encodes the erased self type. Hence if we are
    /// making an object `Foo<Trait>` from a value of type `Foo<T>`, then
    /// `trait_ref` would map `T:Trait`.
    pub fn get_vtable(
        &mut self,
        ty: Ty<'tcx>,
        trait_ref: ty::PolyTraitRef<'tcx>,
    ) -> EvalResult<'tcx, MemoryPointer> {
        debug!("get_vtable(trait_ref={:?})", trait_ref);

        let size = self.type_size(trait_ref.self_ty())?.expect(
            "can't create a vtable for an unsized type",
        );
        let align = self.type_align(trait_ref.self_ty())?;

        let ptr_size = self.memory.pointer_size();
        let methods = ::traits::get_vtable_methods(self.tcx, trait_ref);
        let vtable = self.memory.allocate(
            ptr_size * (3 + methods.count() as u64),
            ptr_size,
            None,
        )?;

        let drop = eval_context::resolve_drop_in_place(self.tcx, ty);
        let drop = self.memory.create_fn_alloc(drop);
        self.memory.write_ptr_sized_unsigned(vtable, PrimVal::Ptr(drop))?;

        let size_ptr = vtable.offset(ptr_size, &self)?;
        self.memory.write_ptr_sized_unsigned(size_ptr, PrimVal::Bytes(size as u128))?;
        let align_ptr = vtable.offset(ptr_size * 2, &self)?;
        self.memory.write_ptr_sized_unsigned(align_ptr, PrimVal::Bytes(align as u128))?;

        for (i, method) in ::traits::get_vtable_methods(self.tcx, trait_ref).enumerate() {
            if let Some((def_id, substs)) = method {
                let instance = eval_context::resolve(self.tcx, def_id, substs);
                let fn_ptr = self.memory.create_fn_alloc(instance);
                let method_ptr = vtable.offset(ptr_size * (3 + i as u64), &self)?;
                self.memory.write_ptr_sized_unsigned(method_ptr, PrimVal::Ptr(fn_ptr))?;
            }
        }

        self.memory.mark_static_initalized(
            vtable.alloc_id,
            Mutability::Mutable,
        )?;

        Ok(vtable)
    }

    pub fn read_drop_type_from_vtable(
        &self,
        vtable: MemoryPointer,
    ) -> EvalResult<'tcx, Option<ty::Instance<'tcx>>> {
        // we don't care about the pointee type, we just want a pointer
        match self.read_ptr(vtable, self.tcx.mk_nil_ptr())? {
            // some values don't need to call a drop impl, so the value is null
            Value::ByVal(PrimVal::Bytes(0)) => Ok(None),
            Value::ByVal(PrimVal::Ptr(drop_fn)) => self.memory.get_fn(drop_fn).map(Some),
            _ => err!(ReadBytesAsPointer),
        }
    }

    pub fn read_size_and_align_from_vtable(
        &self,
        vtable: MemoryPointer,
    ) -> EvalResult<'tcx, (u64, u64)> {
        let pointer_size = self.memory.pointer_size();
        let size = self.memory.read_ptr_sized_unsigned(vtable.offset(pointer_size, self)?)?.to_bytes()? as u64;
        let align = self.memory.read_ptr_sized_unsigned(
            vtable.offset(pointer_size * 2, self)?
        )?.to_bytes()? as u64;
        Ok((size, align))
    }

    pub(crate) fn resolve_associated_const(
        &self,
        def_id: DefId,
        substs: &'tcx Substs<'tcx>,
    ) -> EvalResult<'tcx, ty::Instance<'tcx>> {
        if let Some(trait_id) = self.tcx.trait_of_item(def_id) {
            let trait_ref = ty::Binder(ty::TraitRef::new(trait_id, substs));
            let vtable = self.trans_fulfill_obligation(DUMMY_SP, M::param_env(self), trait_ref)?;
            if let traits::VtableImpl(vtable_impl) = vtable {
                let name = self.tcx.item_name(def_id);
                let assoc_const_opt = self.tcx.associated_items(vtable_impl.impl_def_id).find(
                    |item| {
                        item.kind == ty::AssociatedKind::Const && item.name == name
                    },
                );
                if let Some(assoc_const) = assoc_const_opt {
                    return Ok(ty::Instance::new(assoc_const.def_id, vtable_impl.substs));
                }
            }
        }
        Ok(ty::Instance::new(def_id, substs))
    }
}
