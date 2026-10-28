// 8-bit shift register: load sets q to d; otherwise mode 1 shifts left (sin enters at bit 0), mode 2 shifts
// right (sin enters at bit 7), modes 0 and 3 hold.
module shift_reg(input clk, input load, input [7:0] d, input [1:0] mode, input sin, output reg [7:0] q);
  always @(posedge clk) begin
    if (load) q <= d;
    else case (mode)
      2'd1: q <= {q[6:0], sin};
      2'd2: q <= {sin, q[7:1]};
      default: q <= q;
    endcase
  end
endmodule
