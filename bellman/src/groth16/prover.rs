use crate::log::Stopwatch;

use rand::Rng;

use std::sync::Arc;

use crate::pairing::{CurveAffine, CurveProjective, Engine};

use crate::pairing::ff::{Field, PrimeField};

use super::{ParameterSource, Proof};

use crate::{Circuit, ConstraintSystem, Index, LinearCombination, SynthesisError, Variable};

use crate::domain::{EvaluationDomain, Scalar};

use crate::source::{DensityTracker, FullDensity};

use crate::multiexp::*;

use crate::worker::Worker;

fn eval<E: Engine>(
    lc: &LinearCombination<E>,
    mut input_density: Option<&mut DensityTracker>,
    mut aux_density: Option<&mut DensityTracker>,
    input_assignment: &[E::Fr],
    aux_assignment: &[E::Fr],
) -> E::Fr {
    let mut acc = E::Fr::zero();

    for &(index, coeff) in lc.0.iter() {
        let mut tmp;

        match index {
            Variable(Index::Input(i)) => {
                tmp = input_assignment[i];
                if let Some(ref mut v) = input_density {
                    v.inc(i);
                }
            }
            Variable(Index::Aux(i)) => {
                tmp = aux_assignment[i];
                if let Some(ref mut v) = aux_density {
                    v.inc(i);
                }
            }
        }

        if coeff == E::Fr::one() {
            acc.add_assign(&tmp);
        } else {
            tmp.mul_assign(&coeff);
            acc.add_assign(&tmp);
        }
    }

    acc
}

pub(crate) fn field_elements_into_representations<E: Engine>(
    worker: &Worker,
    scalars: Vec<E::Fr>,
) -> Result<Vec<<E::Fr as PrimeField>::Repr>, SynthesisError> {
    let mut representations = vec![<E::Fr as PrimeField>::Repr::default(); scalars.len()];
    worker.scope(scalars.len(), |scope, chunk| {
        for (scalar, repr) in scalars.chunks(chunk).zip(representations.chunks_mut(chunk)) {
            scope.spawn(move |_| {
                for (scalar, repr) in scalar.iter().zip(repr.iter_mut()) {
                    *repr = scalar.into_repr();
                }
            });
        }
    });

    Ok(representations)
}

pub(crate) fn scalars_into_representations<E: Engine>(
    worker: &Worker,
    scalars: Vec<Scalar<E>>,
) -> Result<Vec<<E::Fr as PrimeField>::Repr>, SynthesisError> {
    let mut representations = vec![<E::Fr as PrimeField>::Repr::default(); scalars.len()];
    worker.scope(scalars.len(), |scope, chunk| {
        for (scalar, repr) in scalars.chunks(chunk).zip(representations.chunks_mut(chunk)) {
            scope.spawn(move |_| {
                for (scalar, repr) in scalar.iter().zip(repr.iter_mut()) {
                    *repr = scalar.0.into_repr();
                }
            });
        }
    });

    Ok(representations)
}

// This is a proving assignment with densities precalculated
pub struct PreparedProver<E: Engine> {
    pub assignment: ProvingAssignment<E>,
}

#[derive(Clone)]
pub struct ProvingAssignment<E: Engine> {
    // Density of queries
    pub a_aux_density: DensityTracker,
    pub b_input_density: DensityTracker,
    pub b_aux_density: DensityTracker,

    // Evaluations of A, B, C polynomials
    a: Vec<Scalar<E>>,
    b: Vec<Scalar<E>>,
    c: Vec<Scalar<E>>,

    // Assignments of variables
    input_assignment: Vec<E::Fr>,
    aux_assignment: Vec<E::Fr>,
}

pub fn prepare_prover<E, C>(circuit: C) -> Result<PreparedProver<E>, SynthesisError>
where
    E: Engine,
    C: Circuit<E>,
{
    let mut prover = ProvingAssignment {
        a_aux_density: DensityTracker::new(),
        b_input_density: DensityTracker::new(),
        b_aux_density: DensityTracker::new(),
        a: vec![],
        b: vec![],
        c: vec![],
        input_assignment: vec![],
        aux_assignment: vec![],
    };

    prover.alloc_input(|| "", || Ok(E::Fr::one()))?;

    circuit.synthesize(&mut prover)?;

    for i in 0..prover.input_assignment.len() {
        prover.enforce(|| "", |lc| lc + Variable(Index::Input(i)), |lc| lc, |lc| lc);
    }

    let prepared = PreparedProver { assignment: prover };

    return Ok(prepared);
}

impl<E: Engine> PreparedProver<E> {
    pub fn create_random_proof<R, P: ParameterSource<E>>(
        self,
        params: P,
        rng: &mut R,
    ) -> Result<Proof<E>, SynthesisError>
    where
        R: Rng,
    {
        let r = rng.gen();
        let s = rng.gen();

        self.create_proof(params, r, s)
    }

    pub fn create_proof<P: ParameterSource<E>>(
        self,
        mut params: P,
        r: E::Fr,
        s: E::Fr,
    ) -> Result<Proof<E>, SynthesisError> {
        let prover = self.assignment;
        let worker = Worker::new();

        let vk = params.get_vk(prover.input_assignment.len())?;

        let _stopwatch = Stopwatch::new();

        let h = {
            let mut a = EvaluationDomain::from_coeffs(prover.a)?;
            let mut b = EvaluationDomain::from_coeffs(prover.b)?;
            let mut c = EvaluationDomain::from_coeffs(prover.c)?;
            elog_verbose!("H query domain size is {}", a.as_ref().len());

            // here a coset is a domain where denominator (z) does not vanish
            // inverse FFT is an interpolation
            a.ifft(&worker);
            // evaluate in coset
            a.coset_fft(&worker);
            // same is for B and C
            b.ifft(&worker);
            b.coset_fft(&worker);
            c.ifft(&worker);
            c.coset_fft(&worker);

            // do A*B-C in coset
            a.mul_assign(&worker, &b);
            drop(b);
            a.sub_assign(&worker, &c);
            drop(c);
            // z does not vanish in coset, so we divide by non-zero
            a.divide_by_z_on_coset(&worker);
            // interpolate back in coset
            a.icoset_fft(&worker);
            let mut a = a.into_coeffs();
            let a_len = a.len() - 1;
            a.truncate(a_len);
            // TODO: parallelize if it's even helpful
            // TODO: in large settings it may worth to parallelize
            let a = Arc::new(scalars_into_representations::<E>(&worker, a)?);
            // let a = Arc::new(a.into_iter().map(|s| s.0.into_repr()).collect::<Vec<_>>());

            multiexp(&worker, params.get_h(a.len())?, FullDensity, a)
        };

        elog_verbose!(
            "{} seconds for prover for H evaluation (mostly FFT)",
            _stopwatch.elapsed()
        );

        let _stopwatch = Stopwatch::new();

        // TODO: Check that difference in operations for different chunks is small
        #[cfg(not(feature = "nolog"))]
        {
            let input_len = prover.input_assignment.len();
            let aux_len = prover.aux_assignment.len();

            elog_verbose!(
                "H quey is densein G1,\nOther queriesare {} elements in G1 and {} elements in G2",
                2 * (input_len + aux_len) + aux_len,
                input_len + aux_len
            );
        }

        let input_assignment = Arc::new(field_elements_into_representations::<E>(
            &worker,
            prover.input_assignment,
        )?);
        let aux_assignment = Arc::new(field_elements_into_representations::<E>(
            &worker,
            prover.aux_assignment,
        )?);

        // TODO: parallelize if it's even helpful
        // TODO: in large settings it may worth to parallelize
        // let input_assignment = Arc::new(prover.input_assignment.into_iter().map(|s| s.into_repr()).collect::<Vec<_>>());
        // let aux_assignment = Arc::new(prover.aux_assignment.into_iter().map(|s| s.into_repr()).collect::<Vec<_>>());

        // let input_len = input_assignment.len();
        // let aux_len = aux_assignment.len();

        // Run a dedicated process for dense vector
        let l = multiexp(
            &worker,
            params.get_l(aux_assignment.len())?,
            FullDensity,
            aux_assignment.clone(),
        );

        let a_aux_density_total = prover.a_aux_density.get_total_density();

        let (a_inputs_source, a_aux_source) =
            params.get_a(input_assignment.len(), a_aux_density_total)?;

        let a_inputs = multiexp(
            &worker,
            a_inputs_source,
            FullDensity,
            input_assignment.clone(),
        );
        let a_aux = multiexp(
            &worker,
            a_aux_source,
            Arc::new(prover.a_aux_density),
            aux_assignment.clone(),
        );

        let b_input_density = Arc::new(prover.b_input_density);
        let b_input_density_total = b_input_density.get_total_density();
        let b_aux_density = Arc::new(prover.b_aux_density);
        let b_aux_density_total = b_aux_density.get_total_density();

        let (b_g1_inputs_source, b_g1_aux_source) =
            params.get_b_g1(b_input_density_total, b_aux_density_total)?;

        let b_g1_inputs = multiexp(
            &worker,
            b_g1_inputs_source,
            b_input_density.clone(),
            input_assignment.clone(),
        );
        let b_g1_aux = multiexp(
            &worker,
            b_g1_aux_source,
            b_aux_density.clone(),
            aux_assignment.clone(),
        );

        let (b_g2_inputs_source, b_g2_aux_source) =
            params.get_b_g2(b_input_density_total, b_aux_density_total)?;

        let b_g2_inputs = multiexp(
            &worker,
            b_g2_inputs_source,
            b_input_density,
            input_assignment,
        );
        let b_g2_aux = multiexp(&worker, b_g2_aux_source, b_aux_density, aux_assignment);

        if vk.delta_g1.is_zero() || vk.delta_g2.is_zero() {
            // If this element is zero, someone is trying to perform a
            // subversion-CRS attack.
            return Err(SynthesisError::UnexpectedIdentity);
        }

        let mut g_a = vk.delta_g1.mul(r);
        g_a.add_assign_mixed(&vk.alpha_g1);
        let mut g_b = vk.delta_g2.mul(s);
        g_b.add_assign_mixed(&vk.beta_g2);
        let mut g_c;
        {
            let mut rs = r;
            rs.mul_assign(&s);

            g_c = vk.delta_g1.mul(rs);
            g_c.add_assign(&vk.alpha_g1.mul(s));
            g_c.add_assign(&vk.beta_g1.mul(r));
        }
        let mut a_answer = a_inputs.wait()?;
        a_answer.add_assign(&a_aux.wait()?);
        g_a.add_assign(&a_answer);
        a_answer.mul_assign(s);
        g_c.add_assign(&a_answer);

        let mut b1_answer = b_g1_inputs.wait()?;
        b1_answer.add_assign(&b_g1_aux.wait()?);
        let mut b2_answer = b_g2_inputs.wait()?;
        b2_answer.add_assign(&b_g2_aux.wait()?);

        g_b.add_assign(&b2_answer);
        b1_answer.mul_assign(r);
        g_c.add_assign(&b1_answer);
        g_c.add_assign(&h.wait()?);
        g_c.add_assign(&l.wait()?);

        elog_verbose!(
            "{} seconds for prover for point multiplication",
            _stopwatch.elapsed()
        );

        Ok(Proof {
            a: g_a.into_affine(),
            b: g_b.into_affine(),
            c: g_c.into_affine(),
        })
    }
}

impl<E: Engine> ConstraintSystem<E> for ProvingAssignment<E> {
    type Root = Self;

    fn alloc<F, A, AR>(&mut self, _: A, f: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError>,
        A: FnOnce() -> AR,
        AR: Into<String>,
    {
        self.aux_assignment.push(f()?);
        self.a_aux_density.add_element();
        self.b_aux_density.add_element();

        Ok(Variable(Index::Aux(self.aux_assignment.len() - 1)))
    }

    fn alloc_input<F, A, AR>(&mut self, _: A, f: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError>,
        A: FnOnce() -> AR,
        AR: Into<String>,
    {
        self.input_assignment.push(f()?);
        self.b_input_density.add_element();

        Ok(Variable(Index::Input(self.input_assignment.len() - 1)))
    }

    fn enforce<A, AR, LA, LB, LC>(&mut self, _: A, a: LA, b: LB, c: LC)
    where
        A: FnOnce() -> AR,
        AR: Into<String>,
        LA: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
        LB: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
        LC: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
    {
        let a = a(LinearCombination::zero());
        let b = b(LinearCombination::zero());
        let c = c(LinearCombination::zero());

        self.a.push(Scalar(eval(
            &a,
            // Inputs have full density in the A query
            // because there are constraints of the
            // form x * 0 = 0 for each input.
            None,
            Some(&mut self.a_aux_density),
            &self.input_assignment,
            &self.aux_assignment,
        )));
        self.b.push(Scalar(eval(
            &b,
            Some(&mut self.b_input_density),
            Some(&mut self.b_aux_density),
            &self.input_assignment,
            &self.aux_assignment,
        )));
        self.c.push(Scalar(eval(
            &c,
            // There is no C polynomial query,
            // though there is an (beta)A + (alpha)B + C
            // query for all aux variables.
            // However, that query has full density.
            None,
            None,
            &self.input_assignment,
            &self.aux_assignment,
        )));
    }

    fn push_namespace<NR, N>(&mut self, _: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR,
    {
        // Do nothing; we don't care about namespaces in this context.
    }

    fn pop_namespace(&mut self) {
        // Do nothing; we don't care about namespaces in this context.
    }

    fn get_root(&mut self) -> &mut Self::Root {
        self
    }
}

pub fn create_random_proof<E, C, R, P: ParameterSource<E>>(
    circuit: C,
    params: P,
    rng: &mut R,
) -> Result<Proof<E>, SynthesisError>
where
    E: Engine,
    C: Circuit<E>,
    R: Rng,
{
    let r = rng.gen();
    let s = rng.gen();

    create_proof::<E, C, P>(circuit, params, r, s)
}

pub fn create_proof<E, C, P: ParameterSource<E>>(
    circuit: C,
    params: P,
    r: E::Fr,
    s: E::Fr,
) -> Result<Proof<E>, SynthesisError>
where
    E: Engine,
    C: Circuit<E>,
{
    let prover = prepare_prover(circuit)?;

    prover.create_proof(params, r, s)
}
