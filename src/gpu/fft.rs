use crate::bls::Engine;
use crate::gpu::{
    error::{GPUError, GPUResult},
    locks, sources,
};
use ff::Field;
use log::info;
use rust_gpu_tools::*;
use std::cmp;

const LOG2_MAX_ELEMENTS: usize = 32; // At most 2^32 elements is supported.
const MAX_LOG2_RADIX: u32 = 8; // Radix256
const MAX_LOG2_LOCAL_WORK_SIZE: u32 = 7; // 128

#[allow(clippy::upper_case_acronyms)]
pub struct FFTKernel<E>
where
    E: Engine,
{
    program: opencl::Program,
    pq_buffer: opencl::Buffer<E::Fr>,
    omegas_buffer: opencl::Buffer<E::Fr>,
    _lock: locks::GPULock, // RFC 1857: struct fields are dropped in the same order as they are declared.
    priority: bool,
}

impl<E> FFTKernel<E>
where
    E: Engine,
{
    pub fn create(priority: bool) -> GPUResult<FFTKernel<E>> {
        let lock = locks::GPULock::lock();
        let id = lock.id(); // Added by jackoelv for C2 20210330

        let devices = opencl::Device::all();
        if devices.is_empty() {
            return Err(GPUError::Simple("No working GPUs found!"));
        }

        // Select the first device for FFT
        // let device = devices[0];
        // Modified by jackoelv for C2 20210330
        // let device = devices[0].clone();
        let device = devices.into_iter().filter(|d| d.bus_id().unwrap() == id).next().unwrap().clone();

        let src = sources::kernel::<E>(device.vendor() == opencl::Vendor::Nvidia);

        let program = opencl::Program::from_opencl(&device, &src)?;
        let pq_buffer = program.create_buffer::<E::Fr>(1 << MAX_LOG2_RADIX >> 1)?;
        let omegas_buffer = program.create_buffer::<E::Fr>(LOG2_MAX_ELEMENTS)?;

        info!("FFT: 1 working device(s) selected.");
        // info!("FFT: Device 0: {}", device.name());
        // Modified by jackoelv for C2 20210330
        // info!("FFT: Device 0: {}", program.device().name());
        info!("FFT: Device {}: {}", id, program.device().name());

        Ok(FFTKernel {
            program,
            pq_buffer,
            omegas_buffer,
            _lock: lock,
            priority,
        })
    }

    /// Peforms a FFT round
    /// * `log_n` - Specifies log2 of number of elements
    /// * `log_p` - Specifies log2 of `p`, (http://www.bealto.com/gpu-fft_group-1.html)
    /// * `deg` - 1=>radix2, 2=>radix4, 3=>radix8, ...
    /// * `max_deg` - The precalculated values pq` and `omegas` are valid for radix degrees up to `max_deg`
    fn radix_fft_round(
        &mut self,
        src_buffer: &opencl::Buffer<E::Fr>,
        dst_buffer: &opencl::Buffer<E::Fr>,
        log_n: u32,
        log_p: u32,
        deg: u32,
        max_deg: u32,
    ) -> GPUResult<()> {
        // Modified by long 20210816
        // if locks::PriorityLock::should_break(self.priority) {
        //     return Err(GPUError::GPUTaken);
        // }
        let _ = self.priority;

        let n = 1u32 << log_n;
        let local_work_size = 1 << cmp::min(deg - 1, MAX_LOG2_LOCAL_WORK_SIZE);
        let global_work_size = n >> deg;
        let kernel = self.program.create_kernel(
            "radix_fft",
            global_work_size as usize,
            local_work_size as usize,
        )?;
        kernel
            .arg(src_buffer)
            .arg(dst_buffer)
            .arg(&self.pq_buffer)
            .arg(&self.omegas_buffer)
            .arg(&opencl::LocalBuffer::<E::Fr>::new(1 << deg))
            .arg(&n)
            .arg(&log_p)
            .arg(&deg)
            .arg(&max_deg)
            .run()?;
        Ok(())
    }

    /// Share some precalculated values between threads to boost the performance
    fn setup_pq_omegas(&mut self, omega: &E::Fr, n: usize, max_deg: u32) -> GPUResult<()> {
        // Precalculate:
        // [omega^(0/(2^(deg-1))), omega^(1/(2^(deg-1))), ..., omega^((2^(deg-1)-1)/(2^(deg-1)))]
        let mut pq = vec![E::Fr::zero(); 1 << max_deg >> 1];
        let twiddle = omega.pow([(n >> max_deg) as u64]);
        pq[0] = E::Fr::one();
        if max_deg > 1 {
            pq[1] = twiddle;
            for i in 2..(1 << max_deg >> 1) {
                pq[i] = pq[i - 1];
                pq[i].mul_assign(&twiddle);
            }
        }
        self.program.write_from_buffer(&self.pq_buffer, 0, &pq)?;

        // Precalculate [omega, omega^2, omega^4, omega^8, ..., omega^(2^31)]
        let mut omegas = vec![E::Fr::zero(); 32];
        omegas[0] = *omega;
        for i in 1..LOG2_MAX_ELEMENTS {
            omegas[i] = omegas[i - 1].pow([2u64]);
        }
        self.program
            .write_from_buffer(&self.omegas_buffer, 0, &omegas)?;

        Ok(())
    }

    /// Performs FFT on `a`
    /// * `omega` - Special value `omega` is used for FFT over finite-fields
    /// * `log_n` - Specifies log2 of number of elements
    pub fn radix_fft(&mut self, a: &mut [E::Fr], omega: &E::Fr, log_n: u32) -> GPUResult<()> {
        let n = 1 << log_n;
        // let mut src_buffer = self.program.create_buffer::<E::Fr>(n)?;
        // let mut dst_buffer = self.program.create_buffer::<E::Fr>(n)?;
        let mut buffer0 = self.program.create_buffer::<E::Fr>(n)?;
        let mut buffer1 = self.program.create_buffer::<E::Fr>(n)?;

        let max_deg = cmp::min(MAX_LOG2_RADIX, log_n);
        self.setup_pq_omegas(omega, n, max_deg)?;

        // self.program.write_from_buffer(&src_buffer, 0, &*a)?;
        buffer0.write_from(0, &*a)?; // Why NOT a ? by long
        let mut log_p = 0u32;
        let mut flag = false;
        while log_p < log_n {
            let deg = cmp::min(max_deg, log_n - log_p);
            flag = !flag;
            if flag == true {
                self.radix_fft_round(&buffer0, &mut buffer1, log_n, log_p, deg, max_deg)?;
            } else {
                self.radix_fft_round(&buffer1, &mut buffer0, log_n, log_p, deg, max_deg)?;
            }
            // self.radix_fft_round(&src_buffer, &dst_buffer, log_n, log_p, deg, max_deg)?;
            log_p += deg;
            // std::mem::swap(&mut src_buffer, &mut dst_buffer);
        }

        // self.program.read_into_buffer(&src_buffer, 0, a)?;
        if flag == true {
            buffer1.read_into(0, a)?;
        } else {
            buffer0.read_into(0, a)?;
        }

        Ok(())
    }
}
